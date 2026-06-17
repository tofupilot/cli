//! # execution-engine
//!
//! Test execution engine that powers [TofuPilot](https://tofupilot.com) procedures.
//!
//! Loads a procedure definition, schedules its phases and plugs across workers,
//! and streams structured events ([`ExecutionEvent`]) to any [`EventSink`]. The
//! engine handles Python and native executable runtimes, operator UI requests,
//! per-job timeouts, and graceful shutdown.
//!
//! ## Main entry points
//!
//! - [`orchestrator::Orchestrator`] — top-level runtime that drives a full procedure
//! - [`procedure::loader::load_procedure_definition`] — parse `procedure.yaml`
//! - [`identify`] — resolve unit identity via operator prompt
//! - [`EventSink`] — sink trait to consume execution events
//!
//! ## Pre-1.0
//!
//! The public API may change between minor releases. Downstream crates should
//! pin an exact version until 1.0.

pub mod constants;
pub mod event_sink;
pub mod events;
pub mod identify_unit;
pub mod job;
pub mod log;
pub mod manifest;
pub mod measurements;
pub mod monitoring;
pub mod orchestrator;
pub mod path_utils;
pub mod plugs;
pub mod procedure;
pub mod protocol;
pub mod python;
pub mod state;
pub mod transport;
pub mod ui;
pub mod unit;
pub mod worker;

pub use event_sink::{EventSink, ExecutionEvent, MultiSink, NullSink, PlannedPhase, PlannedPlug};
pub use identify_unit::{
    identify, IdentifyError, IdentifyHost, IdentifyHostError, PromptRequest, IDENTIFY_PHASE_KEY,
};
