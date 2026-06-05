//! Shared HTTP error surfacing.
//!
//! Server routes return JSON error bodies in one of two shapes:
//!   - OAuth/device-code flows: `{ "error": "...", "error_description": "..." }`
//!   - tRPC / generic APIs:     `{ "error": "..." }` or `{ "message": "..." }`
//!
//! Callers can't just `.error_for_status()?` those away -- we lose the
//! description. Instead, route every CLI HTTP call through
//! [`describe_error`] so the user sees the server's own message (plus a
//! status-code fallback when the body is missing or malformed).

use reqwest::Response;
use serde::Deserialize;

use crate::error::{CliError, CliResult};

#[derive(Default, Deserialize)]
struct ErrorBody {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// Pull a human message out of a non-2xx response. Consumes the body.
/// Never fails -- a malformed body degrades to just the status code.
pub async fn describe_error(resp: Response) -> String {
    let status = resp.status();
    let body: ErrorBody = resp.json().await.unwrap_or_default();

    // Prefer human-readable description, then machine code, then status.
    if let Some(desc) = body.error_description {
        if !desc.is_empty() {
            return match body.error {
                Some(code) if !code.is_empty() => format!("{desc} ({code})"),
                _ => desc,
            };
        }
    }
    if let Some(msg) = body.message {
        if !msg.is_empty() {
            return msg;
        }
    }
    if let Some(code) = body.error {
        if !code.is_empty() {
            return format!("{code} ({status})");
        }
    }
    status.to_string()
}

/// Short-circuit: if the response is 2xx, return it untouched; otherwise
/// return a [`CliError::Status`] carrying the status code (for retry
/// classification) and the server's human message (for display).
pub async fn ok_or_describe(resp: Response) -> CliResult<Response> {
    if resp.status().is_success() {
        Ok(resp)
    } else {
        let status = resp.status().as_u16();
        Err(CliError::Status {
            status,
            body: describe_error(resp).await,
        })
    }
}
