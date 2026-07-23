//! Station command bridge: handles `StationCommand`s from the dashboard (run,
//! cancel, pull) and drives them through the run path.

use station_protocol::{StationCommand, StationEvent};
use tokio::sync::broadcast;

use super::client::StreamClient;
use crate::commands::auth::credentials::Credentials;

/// Stream bridge for standalone `tofupilot run`.
///
/// Creates its own stream connection. Publishes events, sends telemetry,
/// and routes UiResponse commands to the execution engine.
///
/// The background task exits naturally when every broadcast sender for
/// event_rx is dropped. `flush()` lets the caller wait for that drain with
/// a bounded timeout; [`Drop`] aborts the task as a fallback.
pub struct StreamBridge {
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl StreamBridge {
    /// Start the bridge. Returns immediately: the connect (streaming-config
    /// fetch + WebSocket handshake) runs in a background task, so the run —
    /// and the local operator UI — never wait on the dashboard link. The
    /// realtime stream is a bonus surface: when the connect succeeds, events
    /// buffered on the subscription (taken by the caller before spawn, 128
    /// slots) drain to the dashboard in order; when it doesn't, the task
    /// warns on stderr and exits, and the run just stays offline. Every
    /// failure branch is announced, because a silently absent bridge cost
    /// days of diagnosis twice (403 swallowed on user-key runs; endless WS
    /// retry on an unreachable realtime endpoint parking the run forever).
    pub fn new(creds: &Credentials, event_rx: broadcast::Receiver<StationEvent>) -> Self {
        Self::new_with_timeout(creds, event_rx, crate::config::timeouts::REALTIME_CONNECT)
    }

    /// [`Self::new`] with an injectable give-up deadline, split out so the
    /// regression tests don't have to wait the production timeout.
    pub(crate) fn new_with_timeout(
        creds: &Credentials,
        event_rx: broadcast::Receiver<StationEvent>,
        deadline: std::time::Duration,
    ) -> Self {
        let creds = creds.clone();
        let handle = tokio::spawn(Self::connect_and_pump(creds, event_rx, deadline));
        Self {
            handle: Some(handle),
        }
    }

    /// Background body: bounded connect, then the publish/telemetry loop.
    /// The deadline exists to *give up and say so*, not to gate anything —
    /// the underlying client resolves `connect()` only on handshake success
    /// and retries a dead transport forever, so without a bound this task
    /// would silently spin for the whole run. Dropping the pending future
    /// on timeout also drops the client, which closes its actor loop — no
    /// retry leaks past this point.
    async fn connect_and_pump(
        creds: Credentials,
        event_rx: broadcast::Receiver<StationEvent>,
        deadline: std::time::Duration,
    ) {
        let mut client = match tokio::time::timeout(deadline, StreamClient::connect(&creds)).await {
            Ok(Ok(Some(c))) => c,
            Ok(Ok(None)) => {
                crate::log::warn(
                    "Realtime streaming not configured on the server — run continues \
                         offline (dashboard live view disabled).",
                );
                return;
            }
            Ok(Err(e)) => {
                crate::log::warn(&format!(
                    "Realtime streaming unavailable — run continues offline \
                         (dashboard live view disabled). Cause: {e}"
                ));
                return;
            }
            Err(_) => {
                crate::log::warn(&format!(
                    "Realtime endpoint did not answer within {}s — run continues \
                         offline (dashboard live view disabled). Check that the \
                         station can reach the realtime server (DNS + WebSockets).",
                    deadline.as_secs()
                ));
                return;
            }
        };

        let inst_id = creds.installation_id.clone().unwrap_or_default();
        let _ = client
            .publish(&super::collect_hardware_event(&inst_id))
            .await;

        let health_pub = client.clone_for_health();
        let health_inst_id = inst_id.clone();
        let event_pub = client.clone_for_health();

        {
            // Telemetry heartbeat
            let telemetry = tokio::spawn(async move {
                let mut interval =
                    tokio::time::interval(crate::config::timeouts::STATION_HEALTH_INTERVAL);
                interval.tick().await;
                loop {
                    interval.tick().await;
                    // `collect_telemetry_event` does sync `sysinfo`
                    // refresh calls; off-load to `spawn_blocking` so a
                    // slow refresh on Pi-class hosts can't stall the
                    // bridge's event-publish select! tick. Mirrors the
                    // station-mode dispatcher's wrap.
                    let inst_id = health_inst_id.clone();
                    let event = match tokio::task::spawn_blocking(move || {
                        super::collect_telemetry_event(&inst_id)
                    })
                    .await
                    {
                        Ok(e) => e,
                        Err(e) => {
                            crate::log::warn(&format!("telemetry task panicked: {e}"));
                            continue;
                        }
                    };
                    let _ = health_pub.publish(&event).await;
                }
            });

            // Event publishing + UiResponse routing
            let mut rx = event_rx;
            loop {
                tokio::select! {
                    event = rx.recv() => {
                        match event {
                            Ok(e) => { let _ = event_pub.publish(&e).await; }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                crate::log::warn(&format!(
                                    "stream bridge lagged {n} event(s)"
                                ));
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                    msg = client.recv() => {
                        match msg {
                            Some(super::client::StreamMsg::Command(
                                StationCommand::UiResponse { request_id, values }
                            )) => {
                                crate::commands::run::ui_response::send(&request_id, values).await;
                            }
                            None => break,
                            _ => {}
                        }
                    }
                }
            }

            telemetry.abort();
            client.disconnect().await;
        }
    }

    /// Wait up to `timeout` for buffered publishes to drain. The caller must
    /// have dropped every broadcast sender for the event channel before
    /// calling this -- the task only exits on channel close.
    pub async fn flush(mut self, timeout: std::time::Duration) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        crate::tasks::drain_or_abort(
            handle,
            timeout,
            "Timed out draining publish queue; some events may have been dropped.",
        )
        .await;
    }
}

impl Drop for StreamBridge {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal fake instance: serves one canned 200 (or error) response for
    /// `GET /api/cli/stream`, then keeps accepting.
    async fn fake_instance(response: String) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let response = response.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock.write_all(response.as_bytes()).await;
                });
            }
        });
        port
    }

    fn ok_response(stream_url: &str) -> String {
        let body = format!(
            r#"{{"url":"{stream_url}","token":"t","channels":{{"status":"s","commands":"c"}}}}"#
        );
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    }

    fn creds(port: u16) -> Credentials {
        Credentials {
            api_key: "k".into(),
            base_url: format!("http://127.0.0.1:{port}"),
            organization_slug: "test".into(),
            installation_id: Some("inst".into()),
        }
    }

    /// Regression: an unreachable realtime endpoint must never touch the
    /// run. Construction is non-blocking by signature (`new` isn't async —
    /// reintroducing an inline await is a compile-visible change); this
    /// test pins the other half: the background task gives up at its
    /// deadline, so the end-of-run flush completes instead of waiting on a
    /// connect that retries a dead transport forever.
    #[tokio::test]
    async fn unreachable_realtime_never_blocks_construction_or_flush() {
        // Port 1 on loopback: nothing listens, so the WS transport can
        // never hand-shake and the client retries until the deadline.
        let port = fake_instance(ok_response("ws://127.0.0.1:1/connection/websocket")).await;
        let (tx, rx) = broadcast::channel::<StationEvent>(8);

        let bridge =
            StreamBridge::new_with_timeout(&creds(port), rx, std::time::Duration::from_secs(2));

        // End of run: every sender dropped, then a bounded drain. Must
        // resolve once the background task hits its 2s give-up deadline.
        drop(tx);
        tokio::time::timeout(
            std::time::Duration::from_secs(8),
            bridge.flush(std::time::Duration::from_secs(5)),
        )
        .await
        .expect("flush must complete once the connect task gives up, not hang");
    }

    /// The shared connect primitive is bounded: a dead WS transport comes
    /// back as `Err` within the handshake deadline instead of pending
    /// forever. This is what makes the station daemon's boot retry loop
    /// actually cycle on unreachable-realtime networks (its loop only
    /// handles `Err` — an unresolving await used to park the daemon
    /// before the local operator UI ever started).
    #[tokio::test]
    async fn connect_primitive_errors_within_handshake_deadline() {
        let port = fake_instance(ok_response("ws://127.0.0.1:1/connection/websocket")).await;
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            StreamClient::connect_with_timeout(&creds(port), std::time::Duration::from_secs(2)),
        )
        .await
        .expect("connect must resolve within its deadline, not hang");
        assert!(
            res.is_err(),
            "dead transport must surface as Err so caller retry policies engage"
        );
    }

    /// A server-side rejection (e.g. 403 station_auth_required for a user
    /// key) makes the background task exit quickly — and no longer
    /// silently: the error branch warns before returning.
    #[tokio::test]
    async fn rejected_config_fetch_exits_cleanly() {
        let resp = "HTTP/1.1 403 Forbidden\r\ncontent-type: application/json\r\ncontent-length: 33\r\nconnection: close\r\n\r\n{\"error\":\"station_auth_required\"}".to_string();
        let port = fake_instance(resp).await;
        let (tx, rx) = broadcast::channel::<StationEvent>(8);

        let bridge =
            StreamBridge::new_with_timeout(&creds(port), rx, std::time::Duration::from_secs(2));

        drop(tx);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            bridge.flush(std::time::Duration::from_secs(3)),
        )
        .await
        .expect("a rejected config fetch must end the task, not hang the flush");
    }
}
