//! Single source of truth for terminal `StationEvent` emission.
//!
//! Before this module, every emission site (engine `Complete` event,
//! engine crash helper, OpenHTF connector `TestEnd`, OpenHTF connector
//! cancel arm, OpenHTF subprocess-crash arm, outer cancel arm in
//! `run::start`) constructed `StationEvent::RunComplete` / `RunCrashed`
//! struct literals inline. Adding a field to the wire protocol meant
//! finding all of them. This module is the one place that knows the
//! shape; callers pass identity + outcome.

use station_protocol::StationEvent;
use tokio::sync::broadcast;

use super::agent_proto::{AgentProtoCtx, CliEvent};
use super::outcomes;

/// Publish a terminal `RunComplete` for the given run identity. Stamps
/// `execution_id` so consumers can drop late terminals from a cancelled
/// prior run that race the next `RunStarted`. `run_id` is the dashboard
/// id the engine pre-mints for cloud-sync stations; `None` when the run
/// never reached the upload point.
pub fn run_complete(
    tx: &broadcast::Sender<StationEvent>,
    outcome: &str,
    execution_id: &str,
    run_id: Option<String>,
) {
    let _ = tx.send(StationEvent::RunComplete {
        outcome: outcome.to_string(),
        run_id,
        execution_id: Some(execution_id.to_string()),
    });
}

/// Publish a `RunCrashed` followed by a synthetic `RunComplete(ERROR)`.
/// Two events because the wire contract is "every run terminates with
/// `RunComplete`" — consumers that only care about completeness still
/// fire on the synthetic, while consumers that care about the crash
/// detail fold the preceding `RunCrashed`.
///
/// Also enqueues `CliEvent::RunCrashed` for the agent protocol so
/// headless callers see the same signal on stdout.
pub fn run_crashed(
    tx: &broadcast::Sender<StationEvent>,
    agent: Option<&AgentProtoCtx>,
    procedure_id: &str,
    execution_id: &str,
    error_kind: &str,
    error: &str,
    exit_code: i32,
) {
    let _ = tx.send(StationEvent::RunCrashed {
        procedure_id: procedure_id.to_string(),
        error: error.to_string(),
        error_kind: error_kind.to_string(),
        execution_id: Some(execution_id.to_string()),
    });
    run_complete(tx, outcomes::ERROR, execution_id, None);
    if let Some(agent) = agent {
        agent.emitter.enqueue(CliEvent::RunCrashed {
            exit_code,
            stderr_tail: error.to_string(),
        });
    }
}
