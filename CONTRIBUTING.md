# Contributing to the TofuPilot CLI

Thanks for your interest. This document covers building, testing, and the checks
your change must pass.

> This repository is published from the TofuPilot monorepo. Contributions are
> welcome here; maintainers land them upstream and they are mirrored back.

## Prerequisites

- Rust (stable, see `rust-version` in `Cargo.toml` for the MSRV)
- Python 3.9+ with [`uv`](https://github.com/astral-sh/uv) for the agent-protocol
  test harness

The repository is self-contained: the Rust SDK, station protocol, and execution
engine are vendored under `crates/`, so a standalone checkout builds without any
other repository.

## Build

```bash
cargo build
```

## Checks (all must pass before a PR merges)

```bash
cargo fmt --check                          # formatting
cargo clippy --all-targets -- -D warnings  # lints, warnings are errors
cargo test                                 # Rust unit tests
```

CI runs the same three on every pull request (`.github/workflows/test-cli.yml`).

## Agent-protocol tests (Python harness)

End-to-end protocol scenarios live under `tests/agent_protocol/`. They drive the
compiled CLI over the JSON stdin/stdout agent protocol and assert event
sequences across OpenHTF, pytest, and Robot Framework. `ci_smoke.py` is the
subset run in CI.

```bash
cd tests/agent_protocol
# See tests/agent_protocol/README.md for the driver invocation.
```

## Conventions

- Format with `rustfmt` (config in `rustfmt.toml`). No manual style deviations.
- Keep `clippy` clean at `-D warnings`. `clippy::pedantic` is advisory only.
- Comments explain *why*, not *what*; keep them brief.
- CLI command handlers are named `<verb>_cmd` (e.g. `run_cmd`, `login_cmd`,
  `stop_cmd`) to distinguish them from helper verbs in the same module
  (`queue::drop_cmd` vs `queue::drop_one`). The one exception is the
  code-generated `api::*::execute` entrypoints, which the generator owns.
- Prefer error propagation (`?`) over `unwrap()` on any I/O, network, or
  user-input path. `unwrap()`/`expect()` are acceptable only where a failure is a
  genuine invariant violation, and `expect()` should state the invariant.
- The `src/api/*` modules are **code-generated** ("DO NOT EDIT" header). Change
  the generator, not the output.

## Commit messages

Conventional Commits scoped to `cli`, e.g. `fix(cli): ...`, `feat(cli): ...`.
