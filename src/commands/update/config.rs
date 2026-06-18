//! Update-subsystem constants: the version endpoint URL and download timeouts.

pub use crate::config::timeouts::UPDATE_BINARY_DOWNLOAD as DOWNLOAD_TIMEOUT;
pub use crate::config::timeouts::UPDATE_VERSION_FETCH as REQUEST_TIMEOUT;

pub const VERSION_URL: &str = "https://tofupilot.sh/api/cli/version";

/// Total `fetch` send attempts before giving up (1 initial + 2 retries).
/// A transient send-leg reset usually clears on the next call, so two
/// retries is enough; more would only add latency on a genuine outage.
pub const VERSION_FETCH_ATTEMPTS: u32 = 3;

/// Base backoff between transient retries, scaled by the attempt number
/// (200ms, then 400ms). Kept small because a reset clears immediately and
/// this fetch blocks a background startup task.
pub const VERSION_FETCH_BACKOFF: std::time::Duration = std::time::Duration::from_millis(200);
