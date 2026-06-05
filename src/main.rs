//! TofuPilot CLI binary entry point.
//!
//! Parses the command line with clap, then dispatches to the handlers in
//! [`commands`] and the code-generated CRUD commands in [`api`]. With no
//! subcommand and valid credentials it enters station-daemon mode. Also owns
//! process-wide signal handling and the background update check.

// `api/*` is code-generated; the resulting clap-derive enums have one
// large variant (LsArgs with many filter fields) and several tiny ones,
// which trips `clippy::large_enum_variant`. Boxing the big variant
// would help size but would require regenerating the templates and
// updating every callsite to deref. Silence here instead.
#[allow(clippy::large_enum_variant)]
mod api;
mod browser_open;
mod commands;
mod config;
pub mod display;
mod error;
mod http;
mod local_ws;
pub mod log;
mod tasks;

use clap::{Parser, Subcommand};
use commands::update::VERSION;
use tofupilot_sdk::config::ClientConfig;
use tofupilot_sdk::TofuPilot;

macro_rules! api_cmd {
    ($module:path, $command:expr, $json:expr) => {{
        startup();
        let sdk = match get_sdk() {
            Ok(s) => s,
            Err(e) => {
                log::error(&e.to_string());
                std::process::exit(1);
            }
        };
        std::process::exit($module(&sdk, $command, $json).await);
    }};
}

#[derive(Parser)]
#[command(name = "tofupilot", about = "TofuPilot", version)]
struct Cli {
    /// Output JSON instead of human-readable text
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Authenticate via browser or setup token
    Login {
        /// Server URL (defaults to `https://www.tofupilot.app`)
        #[arg(long)]
        url: Option<String>,
        /// Organization slug (skip interactive selection)
        #[arg(long)]
        org: Option<String>,
        /// Station ID (login as station via device flow)
        #[arg(long, conflicts_with = "token")]
        station: Option<String>,
        /// Setup token from dashboard (headless station login, no browser)
        #[arg(long, conflicts_with_all = ["org", "station"])]
        token: Option<String>,
    },
    /// Show the currently logged-in user
    Whoami,
    /// Clear stored credentials
    Logout,
    /// Check for updates and install the latest version
    Update,
    /// Rollback to the previous version
    Rollback,
    /// Pull latest deployments to local workspaces
    Pull,
    /// Run a procedure (from a local path or a pulled deployment)
    Run {
        /// Path to a procedure.yaml, a directory containing one, or a Python entry point.
        /// When set, the procedure runs locally without contacting the dashboard.
        #[arg(value_name = "PATH")]
        path: Option<std::path::PathBuf>,
        /// Pulled deployment ID to run (skip interactive selection).
        /// Mutually exclusive with a positional path.
        #[arg(long, conflicts_with = "path")]
        deployment: Option<String>,
        /// Pre-baked UI values (JSON file: { phase_key: { component_key: value } })
        #[arg(long, value_name = "FILE")]
        ui_values: Option<std::path::PathBuf>,
        /// UI input timeout in seconds. Phases waiting for a required UI
        /// input longer than this time out and fail. Default: wait forever.
        /// Example: --ui-timeout 600
        #[arg(long, value_name = "SECONDS")]
        ui_timeout: Option<u64>,
        /// Force-enable the in-terminal TUI for this run, overriding the
        /// `terminal_ui` station config. Conflicts with `--no-tui`.
        #[arg(long, conflicts_with = "no_tui")]
        tui: bool,
        /// Force-disable the in-terminal TUI for this run, overriding the
        /// `terminal_ui` station config.
        #[arg(long)]
        no_tui: bool,
        /// Force-enable the local browser kiosk UI for this run, overriding
        /// the `kiosk_ui` station config. Spins up an in-process WebSocket and
        /// static-file server bound to loopback. Conflicts with `--no-kiosk`.
        #[arg(long, conflicts_with = "no_kiosk")]
        kiosk: bool,
        /// Force-disable the local browser kiosk UI for this run,
        /// overriding the `kiosk_ui` station config.
        #[arg(long)]
        no_kiosk: bool,
        /// Skip the auto-bootstrap prompt for missing venvs on local-path
        /// runs. The run fails with the original Python resolution error
        /// instead of offering to provision a venv with `uv venv` + deps
        /// install. Use when you manage the venv yourself via a tool
        /// that doesn't write to `<project>/venv/`.
        #[arg(long)]
        no_bootstrap: bool,
        /// Upload the run to the dashboard for a linked local procedure.
        /// Requires a `procedure.json` in the procedure dir (see
        /// `tofupilot link`) or the `TOFUPILOT_PROCEDURE_ID` env var.
        /// Ignored for `--deployment` runs, which always upload.
        #[arg(long)]
        upload: bool,
    },
    /// Link a local procedure directory to a remote dashboard procedure
    Link {
        /// Procedure directory to link (defaults to the current directory)
        #[arg(value_name = "PATH")]
        path: Option<std::path::PathBuf>,
        /// Remote procedure id or name to link to (skip interactive selection)
        #[arg(long)]
        procedure: Option<String>,
    },
    /// Remove the link from a local procedure directory
    Unlink {
        /// Procedure directory to unlink (defaults to the current directory)
        #[arg(value_name = "PATH")]
        path: Option<std::path::PathBuf>,
    },
    /// Manage the offline upload queue
    Queue {
        #[command(subcommand)]
        command: Option<QueueCommand>,
    },
    /// Manage test runs
    Runs {
        #[command(subcommand)]
        command: api::runs::RunsCommand,
    },
    /// Manage units
    Units {
        #[command(subcommand)]
        command: api::units::UnitsCommand,
    },
    /// Manage procedures
    Procedures {
        #[command(subcommand)]
        command: api::procedures::ProceduresCommand,
    },
    /// Manage stations
    Stations {
        #[command(subcommand)]
        command: api::stations::StationsCommand,
    },
    /// Manage parts
    Parts {
        #[command(subcommand)]
        command: api::parts::PartsCommand,
    },
    /// Manage batches
    Batches {
        #[command(subcommand)]
        command: api::batches::BatchesCommand,
    },
    /// Manage attachments
    Attachments {
        #[command(subcommand)]
        command: api::attachments::AttachmentsCommand,
    },
    /// Manage revisions
    Revisions {
        #[command(subcommand)]
        command: api::revisions::RevisionsCommand,
    },
    /// Manage versions
    Versions {
        #[command(subcommand)]
        command: api::versions::VersionsCommand,
    },
    /// Manage users
    Users {
        #[command(subcommand)]
        command: api::users::UsersCommand,
    },
    /// Import runs from structured or tabular files
    Imports {
        #[command(subcommand)]
        command: api::imports::ImportsCommand,
    },
    /// Show local station configuration
    Config,
    /// Manage the station daemon (systemd / launchd unit)
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Install the station service (writes systemd unit / launchd plist)
    Install {
        /// Remove the service definition instead of installing it
        #[arg(long)]
        disable: bool,
    },
    /// Uninstall TofuPilot and remove all data
    Uninstall {
        /// Keep run data and deployments
        #[arg(long)]
        keep_data: bool,
        /// Skip confirmation prompt
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// Run the station daemon in the foreground (used by systemd / launchd)
    Start,
    /// Stop the running station service
    Stop,
    /// Show the station service status
    Status,
}

#[derive(Subcommand)]
enum QueueCommand {
    /// Force retry all pending uploads
    Retry,
    /// Drop queued entries
    Drop {
        /// Queue ID to drop
        id: Option<String>,
        /// Drop all entries
        #[arg(long)]
        all: bool,
    },
}

/// Process-wide signal handling. Called once from `main`.
///
/// - Restore default SIGPIPE so the CLI exits when its stdout pipe
///   closes (e.g. `tofupilot run | head`) instead of running on
///   while writes silently fail. Rust's default ignores SIGPIPE to
///   make `println!` always succeed; for a CLI that holds an
///   exclusive `redb` lock, that default leaves a zombie holding the
///   lock when the pipe consumer goes away.
/// - Catch SIGTERM and SIGHUP. `tokio::signal::ctrl_c` only covers
///   SIGINT, so `kill <pid>` or closing the terminal tab would
///   otherwise leave the CLI alive. Both trigger an immediate exit;
///   the OS releases the redb file lock on process death.
fn install_global_signal_handlers() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }

        tokio::spawn(async move {
            let Ok(mut sigterm) = signal(SignalKind::terminate()) else {
                return;
            };
            let Ok(mut sighup) = signal(SignalKind::hangup()) else {
                return;
            };
            tokio::select! {
                _ = sigterm.recv() => {},
                _ = sighup.recv() => {},
            }
            // The redb lock is released by the OS on process death.
            std::process::exit(130);
        });
    }
}

#[tokio::main]
async fn main() {
    log::enable_vt();
    install_global_signal_handlers();
    let cli = Cli::parse();
    let json_mode = cli.json;

    match cli.command {
        Some(Commands::Login {
            ref url,
            ref org,
            ref station,
            ref token,
        }) => {
            if let Err(e) = commands::auth::login_cmd(
                url.as_deref(),
                org.as_deref(),
                station.as_deref(),
                token.as_deref(),
            )
            .await
            {
                log::error(&format!("Login failed: {e}"));
                std::process::exit(1);
            }
        }
        Some(Commands::Whoami) => {
            if let Err(e) = commands::auth::whoami_cmd().await {
                log::error(&e.to_string());
                std::process::exit(1);
            }
        }
        Some(Commands::Logout) => {
            if let Err(e) = commands::auth::logout_cmd().await {
                log::error(&format!("Logout failed: {e}"));
                std::process::exit(1);
            }
        }
        Some(Commands::Update) => match commands::update::run_update().await {
            Ok(true) => {}
            Ok(false) => log::success(&format!("Already on the latest version (v{VERSION}).")),
            Err(e) => {
                log::error(&format!("Update failed: {e}"));
                std::process::exit(1);
            }
        },
        Some(Commands::Rollback) => {
            if let Err(e) = commands::update::rollback() {
                log::error(&format!("Rollback failed: {e}"));
                std::process::exit(1);
            }
        }
        Some(Commands::Pull) => {
            startup();
            std::process::exit(commands::pull::run_cmd(json_mode).await);
        }
        Some(Commands::Run {
            ref path,
            ref deployment,
            ref ui_values,
            ui_timeout,
            tui,
            no_tui,
            kiosk,
            no_kiosk,
            no_bootstrap,
            upload,
        }) => {
            startup();
            let agent_opts = commands::run::AgentProtoOptions {
                ui_values: ui_values.clone(),
                ui_timeout_secs: ui_timeout,
            };
            // Tri-state UI overrides: explicit flag wins, otherwise fall
            // back to station config in `commands::run::run`.
            let tui_override = match (tui, no_tui) {
                (true, _) => Some(true),
                (_, true) => Some(false),
                _ => None,
            };
            let kiosk_override = match (kiosk, no_kiosk) {
                (true, _) => Some(true),
                (_, true) => Some(false),
                _ => None,
            };
            let source = if let Some(p) = path.clone() {
                commands::run::RunSource::LocalPath { path: p, upload }
            } else {
                // `--upload` only governs local-path runs; deployment runs
                // always upload. Warn rather than silently swallow the flag.
                if upload {
                    log::warn("--upload is ignored for --deployment runs (they always upload).");
                }
                commands::run::RunSource::Deployment(deployment.clone())
            };
            // Local runs without `--upload` don't require credentials. A
            // linked local run with `--upload` does, since it syncs to the
            // dashboard — require them so the failure is a clear login
            // prompt rather than a silent no-op publisher.
            let creds = match &source {
                commands::run::RunSource::LocalPath { upload: false, .. } => {
                    commands::auth::credentials::load()
                }
                commands::run::RunSource::LocalPath { upload: true, .. }
                | commands::run::RunSource::Deployment(_) => {
                    match commands::auth::credentials::require() {
                        Ok(c) => Some(c),
                        Err(e) => {
                            log::error(&e.to_string());
                            std::process::exit(1);
                        }
                    }
                }
            };
            std::process::exit(
                commands::run::run_cmd(
                    source,
                    json_mode,
                    creds.as_ref(),
                    agent_opts,
                    tui_override,
                    kiosk_override,
                    !no_bootstrap,
                )
                .await,
            );
        }
        Some(Commands::Link {
            ref path,
            ref procedure,
        }) => {
            startup();
            std::process::exit(
                commands::link::link_cmd(path.as_deref(), procedure.as_deref(), json_mode).await,
            );
        }
        Some(Commands::Unlink { ref path }) => {
            startup();
            std::process::exit(commands::link::unlink_cmd(path.as_deref()));
        }
        Some(Commands::Queue { command }) => {
            startup();
            std::process::exit(match command {
                None => commands::run::queue::list_cmd(json_mode).await,
                Some(QueueCommand::Retry) => {
                    let creds = match commands::auth::credentials::require() {
                        Ok(c) => c,
                        Err(e) => {
                            log::error(&e.to_string());
                            std::process::exit(1);
                        }
                    };
                    commands::run::queue::retry_cmd(&creds).await
                }
                Some(QueueCommand::Drop { ref id, all }) => {
                    commands::run::queue::drop_cmd(id.as_deref(), all, json_mode).await
                }
            });
        }
        Some(Commands::Runs { command }) => api_cmd!(api::runs::execute, command, json_mode),
        Some(Commands::Units { command }) => api_cmd!(api::units::execute, command, json_mode),
        Some(Commands::Procedures { command }) => {
            api_cmd!(api::procedures::execute, command, json_mode)
        }
        Some(Commands::Stations { command }) => {
            api_cmd!(api::stations::execute, command, json_mode)
        }
        Some(Commands::Parts { command }) => api_cmd!(api::parts::execute, command, json_mode),
        Some(Commands::Batches { command }) => api_cmd!(api::batches::execute, command, json_mode),
        Some(Commands::Attachments { command }) => {
            api_cmd!(api::attachments::execute, command, json_mode)
        }
        Some(Commands::Revisions { command }) => {
            api_cmd!(api::revisions::execute, command, json_mode)
        }
        Some(Commands::Versions { command }) => {
            api_cmd!(api::versions::execute, command, json_mode)
        }
        Some(Commands::Users { command }) => api_cmd!(api::users::execute, command, json_mode),
        Some(Commands::Imports { command }) => {
            api_cmd!(api::imports::execute, command, json_mode)
        }
        Some(Commands::Config) => match commands::db::open() {
            Ok(db) => match db.list_config() {
                Ok(items) if items.is_empty() => {
                    log::info("No config set.");
                }
                Ok(items) => {
                    for (key, value) in &items {
                        if json_mode {
                            println!("{}", serde_json::json!({"key": key, "value": value}));
                        } else {
                            eprintln!("  {key}={value}");
                        }
                    }
                }
                Err(e) => {
                    log::error(&format!("Failed to read config: {e}"));
                    std::process::exit(1);
                }
            },
            Err(e) => {
                log::error(&format!("Failed to open database: {e}"));
                std::process::exit(1);
            }
        },
        Some(Commands::Service { command }) => {
            match command {
                ServiceCommand::Start => {
                    // Run the daemon in the foreground. systemd /
                    // launchd's ExecStart points here.
                    startup_skip_background_check();
                    match commands::auth::credentials::load() {
                        Some(creds) if creds.installation_id.is_some() => {
                            std::process::exit(commands::station::run_cmd(&creds, json_mode).await);
                        }
                        _ => {
                            log::error("Not logged in as a station. Run `tofupilot login --station <id>` first.");
                            std::process::exit(1);
                        }
                    }
                }
                ServiceCommand::Stop => {
                    std::process::exit(commands::service::stop_cmd(json_mode));
                }
                ServiceCommand::Status => {
                    std::process::exit(commands::service::status_cmd(json_mode));
                }
            }
        }
        Some(Commands::Install { disable }) => {
            std::process::exit(commands::install::run_cmd(!disable, json_mode));
        }
        Some(Commands::Uninstall { keep_data, yes }) => {
            std::process::exit(commands::uninstall::run_cmd(keep_data, yes, json_mode).await);
        }
        None => {
            // No-args is the one-command path: if the user is logged
            // in as a station, do the right thing automatically.
            //   * Service already running → print where to find the
            //     UI and tail-the-logs hint, exit 0.
            //   * Service not running → run the daemon in foreground
            //     so the operator sees live output. Idempotently
            //     installs the unit on first run so reboots come back.
            //   * Not logged in → short usage hint.
            startup_skip_background_check();
            match commands::auth::credentials::load() {
                Some(creds) if creds.installation_id.is_some() => {
                    if commands::service::is_running() {
                        let port = commands::service::local_port();
                        log::success("Station service is already running.");
                        log::info(&format!("Kiosk: http://127.0.0.1:{port}/"));
                        #[cfg(target_os = "linux")]
                        log::info("Logs: journalctl --user -u tofupilot -f");
                        #[cfg(target_os = "macos")]
                        log::info("Logs: ~/Library/Logs/TofuPilot/stdout.log");
                        log::info("Stop:  tofupilot service stop");
                        return;
                    }
                    // Just run the daemon foreground. The supervisor
                    // unit was already installed by `tofupilot login`
                    // (or by an explicit `tofupilot install`); calling
                    // `apply_launch_on_boot` again from here would
                    // fire `systemctl enable --now` and spawn a second
                    // daemon that fights this one for port 7321.
                    std::process::exit(commands::station::run_cmd(&creds, json_mode).await);
                }
                _ => {
                    eprintln!("TofuPilot v{VERSION}");
                    eprintln!();
                    eprintln!("Get started:");
                    eprintln!("  tofupilot login              Authenticate this CLI");
                    eprintln!("  tofupilot run [path]         Run a procedure locally");
                    eprintln!();
                    eprintln!("Run `tofupilot --help` for the full command list.");
                }
            }
        }
    }
}

fn get_sdk() -> crate::error::CliResult<TofuPilot> {
    let creds = commands::auth::credentials::require().map_err(|s| s.to_string())?;
    let config = ClientConfig::new(&creds.api_key).base_url(&creds.base_url);
    Ok(TofuPilot::with_config(config))
}

/// Common startup: enforce min version, then opt-in auto-update steps.
fn startup() {
    startup_inner(true);
}

/// Same as `startup`, but skips the spawned `background_check`. The station
/// daemon (`Service Start`) runs its own synchronous boot-time check inside
/// `commands::station::run`; spawning a second one here races on the staged
/// file path and produces a spurious `ENOENT` "Boot update check failed" warn.
fn startup_skip_background_check() {
    startup_inner(false);
}

fn startup_inner(spawn_background_check: bool) {
    commands::update::enforce_min_version();

    if !commands::update::auto_update_enabled() {
        return;
    }

    if let Err(e) = commands::update::apply_staged() {
        log::warn(&format!("Failed to apply staged update: {e}"));
    }

    // Throttle one-shot CLI checks: skip the network entirely if a check
    // ran within the window. Keeps a burst of commands (and offline runs
    // right after a recent check) from each spawning a fetch. The station
    // daemon passes `spawn_background_check = false` and paces itself.
    //
    // Stamp the attempt *before* spawning: the check is detached and the
    // command typically `process::exit`s before it finishes (and its fetch
    // fails outright when offline), so relying on the post-fetch write to
    // advance the throttle clock would mean it never engages. Recording the
    // attempt up front makes the throttle deterministic for fast and
    // offline commands alike.
    if spawn_background_check && commands::update::cli_check_due() {
        commands::update::mark_cli_checked();
        tokio::spawn(async {
            let _ = commands::update::background_check().await;
        });
    }
}
