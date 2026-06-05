//! Named timeout constants, one per purpose.
//!
//! Rule of thumb: anything longer than a few hundred milliseconds or reused
//! in more than one call site belongs here. A bare `Duration::from_secs(5)`
//! at a call site loses the *why* — this file keeps the rationale next to
//! the number so tuning isn't a spelunking exercise.

use std::time::Duration;

// ---------------------------------------------------------------------------
// Auth / login
// ---------------------------------------------------------------------------

/// Default HTTP client timeout for the auth POST path (/login, /auth/*).
/// Generous because the user is actively waiting and a slow proxy is
/// common; short enough that a dead endpoint fails quickly.
pub const AUTH_CLIENT: Duration = Duration::from_secs(30);

/// Probe timeout for low-stakes auth queries (whoami / server ping). We
/// want these to fail fast so the CLI can fall back to cached credentials
/// instead of spinning.
pub const AUTH_PROBE: Duration = Duration::from_secs(5);

/// How long a cached `whoami` identity is served without a refresh.
/// `whoami` is cache-first: within this window it prints the cached
/// identity and skips the network entirely (instant, offline-safe);
/// past it, it does one bounded `AUTH_PROBE` refresh. Identity (role,
/// org name, email) changes rarely, so a day is plenty.
pub const WHOAMI_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

// ---------------------------------------------------------------------------
// Update checker
// ---------------------------------------------------------------------------

/// Version-check fetch. Must be short: blocks CLI startup in the background.
pub const UPDATE_VERSION_FETCH: Duration = Duration::from_secs(3);

/// Binary download. Needs headroom for a full release tarball on a slow
/// network; 2 minutes is enough for ~20MB on 100kbps.
pub const UPDATE_BINARY_DOWNLOAD: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------
// Run / station / event streaming
// ---------------------------------------------------------------------------

/// Per-publish cap inside the managed publisher loop. If Centrifugo hangs
/// on a single message we don't want that to block the entire drain.
pub const PUBLISH_PER_EVENT: Duration = Duration::from_millis(500);

/// Outer drain budget for the event publisher on shutdown. Together with
/// `PUBLISH_PER_EVENT` this caps how long one stuck call can hold shutdown
/// hostage — at worst, drain budget × per-event cap. Sized for fast bursty
/// runs (pytest fires phase_started + phase_finished + measurement events
/// for every test in milliseconds) where 30+ buffered events all need to
/// land on Centrifugo before the publisher's broadcast loop can exit. A
/// healthy publish takes <50ms, so the typical drain returns immediately;
/// the budget only matters when the network is degraded.
pub const PUBLISH_DRAIN: Duration = Duration::from_secs(10);

/// Station health-probe cadence. 30s is frequent enough to notice loss
/// within a user's attention span without flooding the server.
pub const STATION_HEALTH_INTERVAL: Duration = Duration::from_secs(30);

/// Auth re-probe cadence while a station is running. Less frequent than
/// health because token expiry moves slowly.
pub const STATION_AUTH_PROBE_INTERVAL: Duration = Duration::from_secs(300);

/// Background update-check cadence on a long-running station. Each
/// tick fires `update::background_check`, which always fetches when
/// called — this interval is the only rate limit. Stations favour
/// stability over freshness (updates only *stage* here and apply
/// between runs), so the cadence is deliberately slow.
pub const STATION_UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);

/// Minimum gap between background update checks for one-shot CLI
/// invocations. Unlike the station daemon, every `tofupilot <cmd>` would
/// otherwise fire a fresh check; this throttle dedupes rapid commands
/// (e.g. a dev running `run`/`link` in a loop) down to one network call
/// per window. `enforce_min_version` still runs every time off the
/// cached floor, and explicit `tofupilot update` always bypasses this.
pub const CLI_UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

// ---------------------------------------------------------------------------
// Python / connector / emitter
// ---------------------------------------------------------------------------

/// Grace window to let a Python child exit cleanly after SIGTERM before
/// we SIGKILL it. Most user teardown runs in <1s; 5s absorbs the tail.
pub const PYTHON_GRACEFUL_SHUTDOWN: Duration = Duration::from_secs(5);

/// Grace window for stderr-reader join after the child exits.
pub const STDERR_READER_JOIN: Duration = Duration::from_secs(10);

/// Agent-protocol emitter flush deadline. See PROTOCOL.md — deliberately
/// not configurable.
pub const EMITTER_FLUSH: Duration = Duration::from_secs(5);

/// Best-effort stream-connect deadline during `tofupilot pull`. Keeps
/// the pull path responsive even if the broker is unreachable.
pub const PULL_STREAM_CONNECT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// TUI
// ---------------------------------------------------------------------------

/// Keyboard-event poll tick. 50ms hits the "feels responsive" threshold
/// without burning CPU on an idle run.
pub const TUI_TICK: Duration = Duration::from_millis(50);

/// Grace window between the station channel closing and the TUI tearing
/// down. Gives the operator a moment to read the final frame.
pub const TUI_CLOSE_GRACE: Duration = Duration::from_secs(10);
