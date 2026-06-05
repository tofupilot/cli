//! Semantic-version comparison helpers and the compiled-in `VERSION` constant.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Parse a semver string. Tolerates a leading `v` for tags like
/// `v1.2.3`. Returns `None` on garbage so callers can handle the
/// error explicitly — previously a non-numeric suffix (`-rc1`,
/// `+sha.abc`) was silently dropped, making `is_same("1.2.0-rc1",
/// "1.2.0") == true` and the staging logic blind to prereleases.
fn parse(v: &str) -> Option<semver::Version> {
    semver::Version::parse(v.trim_start_matches('v')).ok()
}

/// True iff `a` is `>=` `b`. If either side fails to parse, return
/// false — refuse to compare garbage rather than silently treating
/// it as "current". Caller surfaces this as "couldn't decide" up the
/// stack (typically: skip the update, log the bad version string).
fn at_least(a: &str, b: &str) -> bool {
    match (parse(a), parse(b)) {
        (Some(a), Some(b)) => a >= b,
        _ => false,
    }
}

pub fn is_newer(a: &str, b: &str) -> bool {
    match (parse(a), parse(b)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

pub fn is_same(a: &str, b: &str) -> bool {
    match (parse(a), parse(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// True when the running `VERSION` is `>=` the given target. Used to decide
/// whether a pending-update marker can be cleared as success after restart.
pub fn version_at_least(target: &str) -> bool {
    at_least(VERSION, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(is_newer("1.0.0", "0.99.99"));
    }

    #[test]
    fn same() {
        assert!(is_same("0.1.0", "v0.1.0"));
        assert!(is_same("0.1.0", "0.1.0"));
        assert!(!is_same("0.1.0", "0.2.0"));
    }

    #[test]
    fn at_least_basic() {
        // Equal -> true.
        assert!(at_least("0.20.3", "0.20.3"));
        // Strictly greater -> true.
        assert!(at_least("0.20.3", "0.20.1"));
        assert!(at_least("0.21.0", "0.20.1"));
        assert!(at_least("1.0.0", "0.20.1"));
        // Strictly less -> false.
        assert!(!at_least("0.20.0", "0.20.1"));
        assert!(!at_least("0.19.99", "0.20.0"));
        assert!(!at_least("0.20.1", "1.0.0"));
        // `v` prefix tolerated.
        assert!(at_least("v0.20.3", "0.20.1"));
        assert!(at_least("0.20.3", "v0.20.1"));
    }

    #[test]
    fn version_at_least_uses_pkg_version() {
        // Helper is a thin wrapper around `at_least(VERSION, target)`.
        // We anchor on VERSION reflexively; full comparison logic is
        // covered in `at_least_basic` which doesn't depend on
        // CARGO_PKG_VERSION.
        assert!(version_at_least(VERSION));
    }
}
