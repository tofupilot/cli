//! Update-subsystem constants: the version endpoint URL and download timeouts.

pub use crate::config::timeouts::UPDATE_BINARY_DOWNLOAD as DOWNLOAD_TIMEOUT;
pub use crate::config::timeouts::UPDATE_VERSION_FETCH as REQUEST_TIMEOUT;

pub const VERSION_URL: &str = "https://tofupilot.sh/api/cli/version";
