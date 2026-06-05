//! Implementations of the CLI subcommands. Each submodule owns one command
//! area; the dispatch lives in `main.rs`.

pub mod auth;
pub mod config;
pub mod db;
pub mod http;
pub mod install;
pub mod link;
pub mod pull;
pub mod run;
pub mod service;
pub mod station;
pub mod uninstall;
pub mod update;
pub mod uv_bootstrap;
