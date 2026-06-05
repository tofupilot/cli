//! Centralized timestamp formatting / parsing.
//!
//! Before this module: `chrono::Utc::now().to_rfc3339()` was inlined at
//! every emit site, `parse_from_rfc3339` at every parse site, and
//! `from_timestamp_millis(...).to_rfc3339_opts(...)` at every conversion
//! site. Each was correct in isolation but a future change (e.g. moving
//! to a fixed-precision Millis format on the wire) would have to chase
//! 5+ files.

use chrono::{DateTime, SecondsFormat, Utc};

/// Wall-clock now in RFC 3339. Used at `StationEvent::UiRequest` emit
/// time so the operator UI can anchor its auto-submit countdown on the
/// engine's timeline rather than receive-time.
pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

/// Parse an RFC 3339 timestamp into a UTC `DateTime`. Returns `None` on
/// any parse error so callers can decide how to fall back (typically
/// substitute `Utc::now()`).
pub fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Epoch millis → RFC 3339 with millisecond precision. The OpenHTF
/// connector's NDJSON stream carries timestamps as i64 millis; the
/// wire format expects RFC 3339, so this is the bridge.
pub fn from_millis(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_millis_epoch_is_zulu_with_millis() {
        assert_eq!(from_millis(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn from_millis_keeps_millisecond_precision() {
        assert_eq!(from_millis(1_500), "1970-01-01T00:00:01.500Z");
    }

    #[test]
    fn parse_rfc3339_round_trips_through_from_millis() {
        let s = from_millis(1_700_000_000_123);
        let parsed = parse_rfc3339(&s).expect("from_millis emits valid rfc3339");
        assert_eq!(parsed.timestamp_millis(), 1_700_000_000_123);
    }

    #[test]
    fn from_millis_negative_is_pre_epoch_not_empty() {
        // -1 ms is representable (1969), so we get a real timestamp, not the
        // unwrap_or_default fallback.
        assert_eq!(from_millis(-1), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn from_millis_out_of_range_falls_back_to_empty() {
        // i64::MIN/MAX overflow chrono's representable range, exercising the
        // unwrap_or_default() fallback branch.
        assert_eq!(from_millis(i64::MAX), "");
        assert_eq!(from_millis(i64::MIN), "");
    }

    #[test]
    fn parse_rfc3339_rejects_garbage() {
        assert!(parse_rfc3339("not-a-timestamp").is_none());
    }

    #[test]
    fn parse_rfc3339_rejects_empty_and_date_only() {
        assert!(parse_rfc3339("").is_none());
        assert!(parse_rfc3339("2024-01-01").is_none());
    }

    #[test]
    fn parse_rfc3339_accepts_zulu() {
        assert!(parse_rfc3339("2024-01-01T12:00:00Z").is_some());
    }

    #[test]
    fn parse_rfc3339_normalizes_offset_to_utc() {
        // 12:00 at +02:00 is 10:00 UTC.
        let parsed = parse_rfc3339("2024-01-01T12:00:00+02:00").unwrap();
        assert_eq!(parsed.to_rfc3339(), "2024-01-01T10:00:00+00:00");
    }
}
