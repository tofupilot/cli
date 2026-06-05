//! NDJSON protocol types for Rust-Python IPC over TCP.
//!
//! These replace the protobuf-generated types. JSON fields that were
//! previously double-serialized (e.g. `value_json: String`) are now
//! `serde_json::Value` to avoid the extra encode/decode round-trip.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// -- Worker protocol (Rust -> Python command, Python -> Rust events) --

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct JobCommand {
    pub job_id: String,
    pub slot_id: String,
    pub phase_name: String,
    pub module: String,
    pub function: String,
    pub plugs: HashMap<String, String>,
    pub timeout_ms: Option<u64>,
    pub retry_count: u32,
    pub retry_limit: u32,
    pub unit_info: Option<UnitInfo>,
    pub phase_results: HashMap<String, String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UnitInfo {
    pub serial_number: Option<String>,
    pub part_number: Option<String>,
    pub revision_number: Option<String>,
    pub batch_number: Option<String>,
    #[serde(default)]
    pub sub_units: HashMap<String, String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum WorkerEvent {
    JobComplete(JobResult),
    Error(ErrorEvent),
    AttachFile(AttachFileEvent),
    AttachData(AttachDataEvent),
    UiUpdate(UiUpdateEvent),
    /// Live log line from the Python phase. Emitted per-line as the phase
    /// runs, so a force-killed phase's logs are still visible to the
    /// orchestrator rather than being lost with the discarded `JobResult`.
    PhaseLogLine(PhaseLogLineEvent),
    /// Live measurement write. Lets observers see values land as Python
    /// sets them, not only bundled in the final JobResult.
    MeasurementRecorded(MeasurementRecordedEvent),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct JobResult {
    pub success: bool,
    pub phase_result_json: Option<String>,
    #[serde(default)]
    pub measurements: Vec<Measurement>,
    #[serde(default)]
    pub logs: Vec<LogEntry>,
    pub error: Option<String>,
    pub exit_code: Option<i32>,
    pub unit_json: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Measurement {
    pub name: String,
    pub value: serde_json::Value,
    pub unit: Option<String>,
    pub timestamp: String,
    pub result: Option<String>,
    pub aggregations: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LogEntry {
    pub timestamp: String,
    pub level: String,
    pub message: String,
    pub file: Option<String>,
    pub line: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ErrorEvent {
    pub message: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MeasurementRecordedEvent {
    pub job_id: String,
    pub name: String,
    /// Value is sent as a JSON string by the Python side to avoid reshaping
    /// the tagged union (`{"Numeric": 5.0}` etc.); the consumer parses it.
    pub value_json: String,
    pub timestamp: String,
    #[serde(default)]
    pub unit: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PhaseLogLineEvent {
    pub job_id: String,
    pub level: String,
    pub message: String,
    pub timestamp: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AttachFileEvent {
    pub job_id: String,
    pub source_path: String,
    pub attachment_name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AttachDataEvent {
    pub job_id: String,
    pub data: String,
    pub attachment_name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UiUpdateEvent {
    pub job_id: String,
    pub action: String,
    pub data_json: String,
}

// -- Plug protocol (Rust -> Python request, Python -> Rust response) --

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum PlugRequest {
    CallMethod(MethodRequest),
    GetStatus,
    Cleanup,
    Shutdown,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MethodRequest {
    pub method: String,
    pub args_json: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PlugResponse {
    pub success: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub result_json: Option<String>,
    #[serde(default)]
    pub state: Option<HashMap<String, String>>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub cleanup_duration_seconds: Option<f64>,
}
