//! Centrifugo WebSocket client for the station daemon: subscribes to the
//! installation channel and publishes `StationEvent`s.

use centrifuge_client::{
    config::get_token_fn, Client, ClientConfig, ClientEvent, ServerPublicationContext,
};
use station_protocol::{StationCommand, StationEvent};
use tokio::sync::mpsc;

use crate::commands::auth::credentials::Credentials;
use crate::http::RequestBuilderExt;

#[derive(Debug, serde::Deserialize)]
struct StreamingConfig {
    url: String,
    token: String,
    channels: StreamingChannels,
}

#[derive(Debug, serde::Deserialize)]
struct StreamingChannels {
    status: String,
    commands: String,
}

/// Messages from the event listener to the station loop.
pub enum StreamMsg {
    Command(StationCommand),
    /// A `StationEvent` published by someone else on this station's
    /// status channel (e.g. a dashboard tab broadcasting operator
    /// presence). The CLI's own publishes echo back here too — the
    /// listener doesn't filter self, consumers either ignore or dedup
    /// on their own identifying fields.
    Event(StationEvent),
    Connected,
    Disconnected,
}

pub struct StreamClient {
    status_channel: String,
    client: Client,
    msg_rx: mpsc::Receiver<StreamMsg>,
}

/// Lightweight handle for publishing events (used by background tasks).
pub struct PublishHandle {
    status_channel: String,
    client: Client,
}

impl StreamClient {
    /// Connect to the realtime broker. The WebSocket handshake is bounded
    /// by [`crate::config::timeouts::REALTIME_CONNECT`] — the underlying
    /// client only resolves `connect()` on handshake success and retries a
    /// dead transport internally, so without this bound every caller
    /// (per-run bridge, station-daemon boot loop) could await forever on
    /// an unreachable endpoint. A timeout comes back as `Err`, which each
    /// caller handles with its own policy (warn-and-continue vs retry).
    ///
    /// Because a failed handshake never resolves `connect()`, the timeout
    /// path is also the only place a transport cause (bad TLS certificate,
    /// refused connection) can be reported — see the event-stream watch in
    /// [`Self::connect_with_timeout`].
    pub async fn connect(creds: &Credentials) -> crate::error::CliResult<Option<Self>> {
        Self::connect_with_timeout(creds, crate::config::timeouts::REALTIME_CONNECT).await
    }

    /// [`Self::connect`] with an injectable handshake deadline, split out
    /// so tests don't have to wait the production timeout.
    pub(crate) async fn connect_with_timeout(
        creds: &Credentials,
        handshake_deadline: std::time::Duration,
    ) -> crate::error::CliResult<Option<Self>> {
        let http = crate::http::client();

        let config = match fetch_streaming_config(http, creds).await? {
            Some(c) => c,
            None => return Ok(None),
        };

        let refresh_creds = creds.clone();
        let refresh_http = http.clone();
        let get_token = get_token_fn(move || {
            let creds = refresh_creds.clone();
            let http = refresh_http.clone();
            async move {
                match fetch_streaming_config(&http, &creds).await {
                    Ok(Some(c)) => Ok(c.token),
                    Ok(None) => Err(centrifuge_client::CentrifugeError::BadConfiguration(
                        "streaming not configured".into(),
                    )),
                    Err(e) => Err(centrifuge_client::CentrifugeError::BadConfiguration(
                        e.to_string(),
                    )),
                }
            }
        });

        let mut client_config = ClientConfig::new(&config.url)
            .get_token(get_token)
            .name("tofupilot-cli")
            .version(env!("CARGO_PKG_VERSION"))
            .token(&config.token);

        // Carries the `--ca-cert` bundle so the realtime link trusts the same
        // certificates as every HTTP call. `None` when none is configured,
        // which leaves tokio-tungstenite's default connector in place.
        if let Some(connector) = crate::http::realtime_connector() {
            client_config = client_config.connector(connector);
        }

        let client = Client::new(client_config);

        let mut events = client.events().map_err(|e| format!("Events: {e}"))?;

        // Watch the event stream during the handshake to capture the transport
        // error. `connect()` cannot report one: the actor parks the reply in
        // `connect_waiters` and only ever answers Ok on success (or
        // ClientDisconnected/ClientClosed) — a TLS failure just retries
        // internally until our deadline, so the cause reaches us only as
        // `ClientEvent::Error`.
        //
        // Everything else read here is REPLAYED to the listener below, never
        // dropped: `on_handshake_success` emits the server-sub events (and any
        // publication batched into the handshake) BEFORE it drains the connect
        // waiters, and this station has no client-side subscriptions — every
        // inbound command arrives as a `ServerPublication`. Discarding them
        // would silently lose commands that raced the handshake.
        let mut last_error: Option<String> = None;
        let mut replay: Vec<ClientEvent> = Vec::new();
        let connected = {
            let connect = client.connect();
            tokio::pin!(connect);
            tokio::time::timeout(handshake_deadline, async {
                loop {
                    tokio::select! {
                        result = &mut connect => return result,
                        event = events.recv() => match event {
                            Some(ClientEvent::Error(ctx)) => {
                                last_error = Some(ctx.error.clone());
                                replay.push(ClientEvent::Error(ctx));
                            }
                            Some(other) => replay.push(other),
                            // Actor gone: let `connect` resolve the error.
                            None => return (&mut connect).await,
                        },
                    }
                }
            })
            .await
        };

        // On timeout the Err return below drops `client`, which closes the
        // actor's command channel and ends its internal retry loop — no
        // background connect leaks past an Err.
        match connected {
            Ok(Ok(())) => {}
            // Terminal (ClientClosed / ClientDisconnected): the connection is
            // gone and no listener is spawned, so `replay` is dropped on
            // purpose — there is nowhere to deliver it.
            Ok(Err(e)) => return Err(format!("Connect: {e}").into()),
            Err(_) => {
                let cause = last_error;
                // Name a certificate failure for what it is — the generic
                // "check DNS" text below would send the operator hunting the
                // wrong problem.
                if let Some(cause) = cause {
                    if is_certificate_error(&cause) {
                        // `ca_cert_configured` only reports whether a PEM file
                        // was supplied — a CA trusted through the system store
                        // reads as "not configured" here, so that branch must
                        // not present `--ca-cert` as the only possible cause.
                        let hint = if crate::http::ca_cert_configured() {
                            "the configured CA certificate does not cover it"
                        } else {
                            "the certificate may be expired or issued for \
                             another hostname; if this instance is behind a \
                             private CA, pass it with \
                             `tofupilot login --ca-cert <path>`"
                        };
                        return Err(format!(
                            "Connect: the realtime endpoint's TLS certificate \
                             is not trusted ({cause}) — {hint}."
                        )
                        .into());
                    }
                    return Err(format!(
                        "Connect: no answer from the realtime endpoint within \
                         {}s (last error: {cause})",
                        handshake_deadline.as_secs()
                    )
                    .into());
                }
                return Err(format!(
                    "Connect: no answer from the realtime endpoint within {}s \
                     (check DNS for the realtime domain and that WebSockets \
                     are allowed)",
                    handshake_deadline.as_secs()
                )
                .into());
            }
        }

        let commands_channel = config.channels.commands.clone();
        let status_channel_clone = config.channels.status.clone();
        let (msg_tx, msg_rx) = mpsc::channel::<StreamMsg>(64);
        tokio::spawn(run_event_listener(
            replay,
            events,
            commands_channel,
            status_channel_clone,
            msg_tx,
        ));

        Ok(Some(Self {
            status_channel: config.channels.status,
            client,
            msg_rx,
        }))
    }

    pub fn clone_for_health(&self) -> PublishHandle {
        PublishHandle {
            status_channel: self.status_channel.clone(),
            client: self.client.clone(),
        }
    }

    pub async fn publish(&self, event: &StationEvent) -> crate::error::CliResult<()> {
        publish_bounded(&self.client, &self.status_channel, event).await
    }

    /// Receive the next message (command, connected, or disconnected).
    /// Returns None only when the event listener is permanently gone.
    pub async fn recv(&mut self) -> Option<StreamMsg> {
        self.msg_rx.recv().await
    }

    pub async fn disconnect(self) {
        let _ = self.client.disconnect().await;
    }
}

impl PublishHandle {
    pub async fn publish(&self, event: &StationEvent) -> crate::error::CliResult<()> {
        publish_bounded(&self.client, &self.status_channel, event).await
    }
}

/// Publish an event, never letting an oversized frame reach the broker.
///
/// Centrifugo caps inbound client frames at `message_size_limit` (65536
/// by default). Over that it closes the connection with code 1009, which
/// the client SDK treats as terminal (`reconnect: false`) and the station
/// daemon's broker supervisor -- a one-shot at boot -- never re-establishes.
/// One oversized event therefore takes the station offline on the dashboard
/// for the rest of the process's life, while runs keep succeeding locally
/// and uploading over HTTP. Silent, permanent, and easy to mistake for a
/// flaky dashboard.
///
/// So an event over budget is degraded (see `StationEvent::shrink_to_fit`:
/// the prompt keeps its inputs and loses its inline image, the log line
/// keeps its context and loses its tail) and only dropped when even that
/// can't fit. Both paths are logged: a publish that silently does nothing
/// is how this class of bug stays invisible.
///
/// Every publish in the CLI funnels through here -- `StreamClient::publish`
/// and `PublishHandle::publish` are the only callers of the broker
/// client's `publish`, so this is the single chokepoint for the guarantee.
async fn publish_bounded(
    client: &Client,
    channel: &str,
    event: &StationEvent,
) -> crate::error::CliResult<()> {
    let data = serde_json::to_vec(event).map_err(|e| format!("Serialize: {e}"))?;

    let data = if data.len() <= station_protocol::MAX_EVENT_BYTES {
        data
    } else {
        let kind = station_event_kind(event);
        match event.shrink_to_fit(station_protocol::MAX_EVENT_BYTES) {
            Some(degraded) => {
                let shrunk =
                    serde_json::to_vec(&degraded).map_err(|e| format!("Serialize: {e}"))?;
                if oversize_first_report(kind, "degraded") {
                    crate::log::warn(&format!(
                        "Event '{kind}' was {} bytes, over the {} byte realtime limit; \
                         published a degraded copy ({} bytes). The station's local \
                         operator UI still shows the full content. Further '{kind}' \
                         degraded-copy reports are suppressed for this process.",
                        data.len(),
                        station_protocol::MAX_EVENT_BYTES,
                        shrunk.len(),
                    ));
                }
                shrunk
            }
            None => {
                if oversize_first_report(kind, "dropped") {
                    crate::log::error(&format!(
                        "Event '{kind}' was {} bytes and could not be reduced under the \
                         {} byte realtime limit; dropped instead of publishing it, which \
                         would have disconnected this station from the dashboard. Further \
                         '{kind}' drop reports are suppressed for this process.",
                        data.len(),
                        station_protocol::MAX_EVENT_BYTES,
                    ));
                }
                return Ok(());
            }
        }
    };

    client
        .publish(channel, data)
        .await
        .map_err(|e| format!("Publish: {e}").into())
}

/// Once-per-kind-and-outcome-per-process gate for the oversize log lines
/// above. A chatty test can produce hundreds of oversized `phase_log`s in
/// one run; repeating the warning for each would flood stderr (and, in TUI
/// mode, scribble over the interface) without adding information. Keyed by
/// outcome as well as kind: a DROP loses content the dashboard never gets,
/// so it must be reported even when an earlier event of the same kind was
/// merely degraded. Returns true only the first time a pair is reported.
fn oversize_first_report(kind: &'static str, outcome: &'static str) -> bool {
    use std::sync::{Mutex, OnceLock};
    type Seen = std::collections::BTreeSet<(&'static str, &'static str)>;
    static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();
    SEEN.get_or_init(Default::default)
        .lock()
        .map(|mut seen| seen.insert((kind, outcome)))
        .unwrap_or(false)
}

/// Discriminant name for the oversize log lines above, so an operator can
/// tell which event was degraded without dumping its (huge) body.
fn station_event_kind(event: &StationEvent) -> &'static str {
    match event {
        StationEvent::UiRequest { .. } => "ui_request",
        StationEvent::UiUpdate { .. } => "ui_update",
        StationEvent::IdentifyRequest { .. } => "identify_request",
        StationEvent::PhaseLog { .. } => "phase_log",
        StationEvent::PlugLog { .. } => "plug_log",
        StationEvent::PhaseComplete { .. } => "phase_complete",
        StationEvent::MeasurementUpdate { .. } => "measurement_update",
        StationEvent::RunCrashed { .. } => "run_crashed",
        _ => "event",
    }
}

/// Cheap discriminant printer for inbound `StationCommand`. Used by the
/// listener log so operators can tell which command landed without
/// dumping the full payload.
fn station_command_kind(cmd: &StationCommand) -> &'static str {
    match cmd {
        StationCommand::Logout { .. } => "Logout",
        StationCommand::ConfigUpdate { .. } => "ConfigUpdate",
        StationCommand::Pull {} => "Pull",
        StationCommand::Run { .. } => "Run",
        StationCommand::UploadRun { .. } => "UploadRun",
        StationCommand::UiResponse { .. } => "UiResponse",
        StationCommand::Kill { .. } => "Kill",
        StationCommand::Stop { .. } => "Stop",
        StationCommand::SkipPhase { .. } => "SkipPhase",
        StationCommand::RetryPhase { .. } => "RetryPhase",
        StationCommand::QueueRetry { .. } => "QueueRetry",
        StationCommand::QueueDrop { .. } => "QueueDrop",
        StationCommand::Exit {} => "Exit",
    }
}

/// Whether a transport error text is a TLS trust failure. Matches on rustls'
/// `Display` output (`"invalid peer certificate: UnknownIssuer"`), which
/// reaches us only as a string inside `ClientEvent::Error` — the typed error
/// is boxed as `dyn Error` by centrifuge-client and then formatted, so there
/// is nothing left to downcast.
fn is_certificate_error(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("certificate") || lower.contains("unknownissuer")
}

async fn run_event_listener(
    // Events observed while watching for a handshake error, replayed here in
    // arrival order so nothing published during the handshake is lost.
    replay: Vec<ClientEvent>,
    mut events: mpsc::Receiver<ClientEvent>,
    commands_channel: String,
    status_channel: String,
    msg_tx: mpsc::Sender<StreamMsg>,
) {
    let mut replay = replay.into_iter();
    while let Some(event) = match replay.next() {
        Some(event) => Some(event),
        None => events.recv().await,
    } {
        // If the station loop dropped its receiver we can't deliver anything;
        // end the listener rather than spinning on silent send errors.
        match event {
            ClientEvent::ServerPublication(ServerPublicationContext {
                channel,
                publication,
            }) => {
                if channel == commands_channel {
                    match serde_json::from_slice::<StationCommand>(&publication.data) {
                        Ok(cmd) => {
                            // Surface inbound commands so a silent dispatcher path
                            // (e.g. Pull arriving while mid-run) is observable in
                            // the operator's terminal. Without this, "nothing
                            // happened" looks identical to "command never arrived".
                            // Skip UiResponse: one fires per prompt answer, which
                            // floods the terminal during interactive runs without
                            // adding diagnostic value -- the prompt resolution is
                            // already implied by the next phase advancing.
                            if !matches!(cmd, StationCommand::UiResponse { .. }) {
                                crate::log::info(&format!(
                                    "Received command: {}",
                                    station_command_kind(&cmd)
                                ));
                            }
                            if msg_tx.send(StreamMsg::Command(cmd)).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            // Unknown command variant (e.g. a newer server sent a
                            // command this CLI doesn't know about). Warn rather
                            // than silently drop so revocation latency is
                            // diagnosable -- on old CLIs this flags a missed
                            // StationCommand::Logout that only the auth probe
                            // will then catch.
                            let snippet = String::from_utf8_lossy(&publication.data);
                            let trimmed: String = snippet.chars().take(120).collect();
                            crate::log::warn(&format!(
                                "Ignoring unknown station command: {e} (payload: {trimmed})"
                            ));
                        }
                    }
                } else if channel == status_channel {
                    // Status channel carries our own publishes (the CLI
                    // publishes telemetry, hardware, run events here and
                    // Centrifugo echoes back to every subscriber
                    // including ourselves). We only care about the
                    // collaborative-presence subset — everything else
                    // was emitted locally and is already in the right
                    // place. Deserialize leniently: unknown variants
                    // coming from a newer web deploy should be ignored,
                    // not warned, because status is a fan-out channel.
                    if let Ok(evt) = serde_json::from_slice::<StationEvent>(&publication.data) {
                        if matches!(evt, StationEvent::Presence(_))
                            && msg_tx.send(StreamMsg::Event(evt)).await.is_err()
                        {
                            break;
                        }
                    }
                }
            }
            ClientEvent::Connected(_) => {
                if msg_tx.send(StreamMsg::Connected).await.is_err() {
                    break;
                }
            }
            ClientEvent::Disconnected(_) if msg_tx.send(StreamMsg::Disconnected).await.is_err() => {
                break;
            }
            _ => {}
        }
    }
}

async fn fetch_streaming_config(
    http: &reqwest::Client,
    creds: &Credentials,
) -> crate::error::CliResult<Option<StreamingConfig>> {
    let base = creds.base();
    let res = http
        .get(format!("{base}/api/cli/stream"))
        .bearer(&creds.api_key)
        .send()
        .await
        .map_err(|e| format!("Fetch streaming config: {e}"))?;

    let status = res.status();
    if !status.is_success() {
        // Distinguish auth failures (revoked / replaced credentials) from
        // server-side missing-config (503) and generic 5xx so the CLI can
        // print a useful next-step instead of "streaming not configured".
        return match status.as_u16() {
            401 | 403 => Err(if creds.installation_id.is_some() {
                format!(
                    "Station logged out. Open {base}/{org}/stations, pick this station, and copy a fresh setup command to reconnect.",
                    org = creds.organization_slug,
                )
            } else {
                "Logged out. Run `tofupilot login` to authenticate again.".to_string()
            }
            .into()),
            503 => Err("Server has streaming disabled. Contact your TofuPilot admin."
                .to_string()
                .into()),
            code => Err(format!("Streaming config fetch failed (HTTP {code}). Check {base} is reachable and try again.").into()),
        };
    }

    let config: StreamingConfig = res
        .json()
        .await
        .map_err(|e| format!("Parse streaming config: {e}"))?;

    Ok(Some(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The handshake watcher consumes events to find a TLS error, so anything
    /// else it sees must be replayed — `on_handshake_success` emits server-sub
    /// events (and publications batched into the handshake) before it resolves
    /// `connect()`, and this station receives every command as a
    /// `ServerPublication`. Dropping them loses commands that raced the
    /// handshake.
    #[tokio::test]
    async fn events_seen_during_the_handshake_reach_the_listener() {
        let (tx, rx) = mpsc::channel::<ClientEvent>(8);
        let (msg_tx, mut msg_rx) = mpsc::channel::<StreamMsg>(8);

        let command = StationCommand::Run {
            procedure_id: Some("proc_1".to_string()),
            reuse_unit: None,
            operated_by: None,
            only_phase: None,
        };
        let replay = vec![ClientEvent::ServerPublication(ServerPublicationContext {
            channel: "commands".to_string(),
            publication: centrifuge_client::Publication {
                data: serde_json::to_vec(&command).unwrap(),
                info: None,
                offset: 0,
                tags: Default::default(),
            },
        })];

        tokio::spawn(run_event_listener(
            replay,
            rx,
            "commands".to_string(),
            "status".to_string(),
            msg_tx,
        ));
        drop(tx);

        match msg_rx.recv().await {
            Some(StreamMsg::Command(StationCommand::Run { procedure_id, .. })) => {
                assert_eq!(procedure_id.as_deref(), Some("proc_1"));
            }
            _ => panic!("replayed command never reached the listener"),
        }
    }

    use super::is_certificate_error;

    /// Pinned to rustls' real `Display` text: `Error::InvalidCertificate`
    /// formats as `"invalid peer certificate: {err}"`, and tungstenite wraps
    /// it again on the way out. If rustls ever rewords these, the operator
    /// silently goes back to the misleading "check DNS" hint.
    #[test]
    fn private_ca_failures_are_recognised_as_certificate_errors() {
        assert!(is_certificate_error(
            "IO error: invalid peer certificate: UnknownIssuer"
        ));
        assert!(is_certificate_error("invalid peer certificate: Expired"));
        assert!(is_certificate_error(
            "invalid peer certificate: NotValidForName"
        ));
    }

    #[test]
    fn unrelated_transport_failures_keep_the_generic_hint() {
        assert!(!is_certificate_error(
            "IO error: failed to lookup address information: Name or service not known"
        ));
        assert!(!is_certificate_error(
            "IO error: Connection refused (os error 111)"
        ));
        assert!(!is_certificate_error("HTTP error: 403 Forbidden"));
    }
}
