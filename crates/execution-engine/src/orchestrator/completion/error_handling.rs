use crate::job::{Job, JobResult};
use crate::log::LogEntry;
use uuid::Uuid;

pub fn convert_error_to_result(error: String, job: &Job, job_id: Uuid) -> JobResult {
    log::error!("Job {} failed: {}", job_id, error);

    let error_logs = vec![LogEntry {
        timestamp: chrono::Utc::now().to_rfc3339(),
        level: "ERROR".to_string(),
        message: error.clone(),
        file: None,
        line: None,
    }];

    if error.contains("timed out") || error.contains("timeout") {
        // Extract the timeout duration (in ms) either from the error string
        // or from the job config, then convert to seconds before storing —
        // `JobResult::new_timeout` expects seconds, and downstream error
        // messages format as "timed out after N seconds".
        let timeout_ms = error
            .split_whitespace()
            .filter_map(|s| s.parse::<u64>().ok())
            .next()
            .or(job.timeout_ms)
            .unwrap_or(0);
        let timeout_secs = (timeout_ms as f64 / 1000.0).round().max(1.0) as u64;

        let mut result = JobResult::new_timeout(timeout_secs);
        result.logs = error_logs;
        result
    } else {
        let mut result = JobResult::new_error(error);
        result.logs = error_logs;
        result
    }
}
