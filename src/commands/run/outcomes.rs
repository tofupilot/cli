//! Canonical outcome string constants shared across all emission sites.
//!
//! The wire protocol uses ASCII upper-case strings (`PASS`, `FAIL`, …).
//! Before this module, three places stringly-typed them independently:
//! the YAML engine sink, the OpenHTF connector, and the final RunFinished
//! emission in `mod.rs`. A typo in any of them was a silent drift — a
//! consumer that cared about one site would miss the divergence.
//!
//! Every Rust emission MUST go through these constants. The Python-side
//! OpenHTF connector mirrors the same names in `openhtf.py`; keep them
//! in lockstep.

use execution_engine::job::Outcome;
use tofupilot_sdk::types::Level;

// Phase/run outcomes on the wire. Keep alphabetical so future additions
// are obviously new.
//
// `XFAIL` and `XPASS` are pytest-specific. XFAIL = expected fail,
// observed fail (xfail marker fired); XPASS = expected fail, observed
// pass under a strict xfail marker (the test "should have failed but
// didn't" — pytest treats that as a real failure). Both ride on the
// wire as distinct strings for live consumers; at the SDK boundary in
// `parse_phase_outcome` XFAIL collapses to Skip and XPASS to Fail.
// OpenHTF and YAML never emit either — only the pytest connector does.
pub const ABORTED: &str = "ABORTED";
pub const ERROR: &str = "ERROR";
pub const FAIL: &str = "FAIL";
pub const PASS: &str = "PASS";
pub const RETRY: &str = "RETRY";
pub const SKIP: &str = "SKIP";
pub const STOP: &str = "STOP";
pub const TIMEOUT: &str = "TIMEOUT";
pub const XFAIL: &str = "XFAIL";
pub const XPASS: &str = "XPASS";

/// Wire string → SDK validator `Outcome` (PASS/FAIL/UNSET). Anything
/// unrecognized maps to UNSET, matching the server's lenient parsing.
pub fn validator_outcome_from_wire(s: &str) -> tofupilot_sdk::types::Outcome {
    use tofupilot_sdk::types::Outcome as SdkOutcome;
    match s {
        PASS => SdkOutcome::Pass,
        FAIL => SdkOutcome::Fail,
        _ => SdkOutcome::Unset,
    }
}

/// Execution-engine `Outcome` → wire string. Panics would be a new
/// variant without a mapping — compile-time `match` exhaustiveness
/// prevents that.
pub fn from_execution_outcome(o: &Outcome) -> &'static str {
    match o {
        Outcome::Pass => PASS,
        Outcome::Fail => FAIL,
        Outcome::Error => ERROR,
        Outcome::Timeout => TIMEOUT,
        Outcome::Stop => STOP,
        Outcome::Skip => SKIP,
        Outcome::Retry => RETRY,
    }
}

/// Final run outcome inferred from the child's exit code when no more
/// detailed signal is available (e.g. the OpenHTF bridge never emitted
/// a `test_end`).
///
/// POSIX signal-range exit codes (128 + N) land as `ABORTED` rather than
/// generic `FAIL` so agents can distinguish "test failed" from "process
/// was killed" without parsing the `run_crashed` event:
///
/// - 130 (SIGINT), 143 (SIGTERM) — user or supervisor asked us to stop.
/// - 137 (SIGKILL), 139 (SIGSEGV), 134 (SIGABRT) — hard crash.
///
/// The detailed signal still lives in `run_crashed.stderr_tail`; this
/// is just a coarser first signal for triage.
pub fn from_exit_code(code: i32) -> &'static str {
    match code {
        0 => PASS,
        // 5 = pytest's "no tests collected" exit. Surface as ERROR
        // because empty collection is a procedure-config bug (a
        // procedure declared pytest but shipped zero `test_*.py`
        // files), not a successful no-op. There's no genuine
        // "skipped run" use case at the run-outcome level, so we
        // don't grow the run_outcome enum for it. OpenHTF / YAML /
        // plain never return 5.
        5 => ERROR,
        // 129..=192 covers SIGHUP through the high-numbered RT signals.
        // Exit code = 128 + signal_number on Unix; on Windows child
        // processes don't carry signal info, so this branch is a no-op
        // there (exit codes stay in a user-defined range).
        129..=192 => ABORTED,
        _ => FAIL,
    }
}

/// Wire-string → SDK `Level`. Centralized so engine and connector log
/// builders agree on the level mapping (and so adding a new level is a
/// single-file change). Unknown strings collapse to `Info` — matches
/// the prior inline behaviour at both call sites.
pub fn parse_log_level(s: &str) -> Level {
    match s {
        "DEBUG" => Level::Debug,
        "WARNING" => Level::Warning,
        "ERROR" => Level::Error,
        "CRITICAL" => Level::Critical,
        _ => Level::Info,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_pass() {
        assert_eq!(from_exit_code(0), PASS);
    }

    #[test]
    fn signal_range_is_aborted() {
        assert_eq!(from_exit_code(130), ABORTED); // SIGINT
        assert_eq!(from_exit_code(134), ABORTED); // SIGABRT
        assert_eq!(from_exit_code(137), ABORTED); // SIGKILL
        assert_eq!(from_exit_code(139), ABORTED); // SIGSEGV
        assert_eq!(from_exit_code(143), ABORTED); // SIGTERM
    }

    #[test]
    fn ordinary_nonzero_is_fail() {
        assert_eq!(from_exit_code(1), FAIL);
        assert_eq!(from_exit_code(2), FAIL);
        assert_eq!(from_exit_code(42), FAIL);
    }

    #[test]
    fn pytest_no_tests_collected_is_error() {
        // pytest exits 5 when collection finds zero tests. Surfaced
        // as ERROR — empty collection is a procedure-config bug, not
        // a genuine skip. Run-outcome enum has no SKIP variant.
        assert_eq!(from_exit_code(5), ERROR);
    }

    #[test]
    fn boundaries() {
        assert_eq!(from_exit_code(128), FAIL); // not in signal range
        assert_eq!(from_exit_code(129), ABORTED); // SIGHUP, start of range
        assert_eq!(from_exit_code(192), ABORTED); // end of range
        assert_eq!(from_exit_code(193), FAIL); // past range
    }

    #[test]
    fn parse_log_level_maps_known_and_falls_back_to_info() {
        assert!(matches!(parse_log_level("DEBUG"), Level::Debug));
        assert!(matches!(parse_log_level("WARNING"), Level::Warning));
        assert!(matches!(parse_log_level("ERROR"), Level::Error));
        assert!(matches!(parse_log_level("CRITICAL"), Level::Critical));
        // Match is case-sensitive by design; unknown/lowercase → Info.
        assert!(matches!(parse_log_level("INFO"), Level::Info));
        assert!(matches!(parse_log_level("debug"), Level::Info));
        assert!(matches!(parse_log_level(""), Level::Info));
    }

    #[test]
    fn validator_outcome_only_pass_fail_are_recognized() {
        use tofupilot_sdk::types::Outcome as SdkOutcome;
        assert!(matches!(
            validator_outcome_from_wire(PASS),
            SdkOutcome::Pass
        ));
        assert!(matches!(
            validator_outcome_from_wire(FAIL),
            SdkOutcome::Fail
        ));
        assert!(matches!(
            validator_outcome_from_wire(ERROR),
            SdkOutcome::Unset
        ));
        assert!(matches!(
            validator_outcome_from_wire("anything"),
            SdkOutcome::Unset
        ));
    }

    #[test]
    fn execution_outcome_round_trips_to_wire_strings() {
        assert_eq!(from_execution_outcome(&Outcome::Pass), PASS);
        assert_eq!(from_execution_outcome(&Outcome::Fail), FAIL);
        assert_eq!(from_execution_outcome(&Outcome::Error), ERROR);
        assert_eq!(from_execution_outcome(&Outcome::Timeout), TIMEOUT);
        assert_eq!(from_execution_outcome(&Outcome::Stop), STOP);
        assert_eq!(from_execution_outcome(&Outcome::Skip), SKIP);
        assert_eq!(from_execution_outcome(&Outcome::Retry), RETRY);
    }
}
