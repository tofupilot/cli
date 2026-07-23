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

        let client_config = ClientConfig::new(&config.url)
            .get_token(get_token)
            .name("tofupilot-cli")
            .version(env!("CARGO_PKG_VERSION"))
            .token(&config.token);

        let client = Client::new(client_config);

        let events = client.events().map_err(|e| format!("Events: {e}"))?;

        // On timeout the Err return below drops `client`, which closes the
        // actor's command channel and ends its internal retry loop — no
        // background connect leaks past an Err.
        match tokio::time::timeout(handshake_deadline, client.connect()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(format!("Connect: {e}").into()),
            Err(_) => {
                return Err(format!(
                    "Connect: no answer from the realtime endpoint within {}s \
                     (check DNS for the realtime domain and that WebSockets \
                     are allowed)",
                    handshake_deadline.as_secs()
                )
                .into())
            }
        }

        let commands_channel = config.channels.commands.clone();
        let status_channel_clone = config.channels.status.clone();
        let (msg_tx, msg_rx) = mpsc::channel::<StreamMsg>(64);
        tokio::spawn(run_event_listener(
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
        let data = serde_json::to_vec(event).map_err(|e| format!("Serialize: {e}"))?;
        self.client
            .publish(&self.status_channel, data)
            .await
            .map_err(|e| format!("Publish: {e}").into())
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
        let data = serde_json::to_vec(event).map_err(|e| format!("Serialize: {e}"))?;
        self.client
            .publish(&self.status_channel, data)
            .await
            .map_err(|e| format!("Publish: {e}").into())
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

async fn run_event_listener(
    mut events: mpsc::Receiver<ClientEvent>,
    commands_channel: String,
    status_channel: String,
    msg_tx: mpsc::Sender<StreamMsg>,
) {
    while let Some(event) = events.recv().await {
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
