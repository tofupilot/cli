//! Plug resource management and lifecycle control.
//!
//! Manages instrument/resource allocation across test phases with proper
//! setup/teardown sequencing and scope-based lifecycle management.
//!
//! # Components
//!
//! - [`manager`]: Resource pool management and allocation
//! - [`guard`]: RAII guards for automatic resource cleanup
//! - [`plug_service`]: Python subprocess management for persistent plugs
//! - [`station_host`]: station-process-scoped ownership of `scope: station` plugs

pub mod guard;
pub mod manager;
pub mod plug_service;
pub mod process;
pub mod station_host;
