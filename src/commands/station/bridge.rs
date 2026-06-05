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
    /// Connect and start publishing. Returns None if streaming not configured.
    pub async fn new(
        creds: &Credentials,
        event_rx: broadcast::Receiver<StationEvent>,
    ) -> Option<Self> {
        let mut client = match StreamClient::connect(creds).await {
            Ok(Some(c)) => c,
            Ok(None) | Err(_) => return None,
        };

        let inst_id = creds.installation_id.clone().unwrap_or_default();
        let _ = client
            .publish(&super::collect_hardware_event(&inst_id))
            .await;

        let health_pub = client.clone_for_health();
        let health_inst_id = inst_id.clone();
        let event_pub = client.clone_for_health();

        let handle = tokio::spawn(async move {
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
        });

        Some(Self {
            handle: Some(handle),
        })
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
