# TofuPilot CLI

[![Test CLI](https://github.com/tofupilot/cli/actions/workflows/test-cli.yml/badge.svg)](https://github.com/tofupilot/cli/actions/workflows/test-cli.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)

Run, deploy, and manage [TofuPilot](https://tofupilot.com) test procedures from the terminal.

## Install

### Pre-built binary (recommended)

```bash
curl -fsSL https://www.tofupilot.app/install | bash
```

Or download a binary from the [latest release](https://github.com/tofupilot/cli/releases/latest) and verify it against the published `sha256` checksums.

| Platform | Architectures |
|----------|---------------|
| macOS    | x86_64, aarch64 |
| Linux    | x86_64, aarch64 |
| Windows  | x86_64 |

### From source

The repository is self-contained: the Rust SDK, station protocol, and
execution engine are vendored under `crates/`, so no other checkout is needed.

```bash
git clone https://github.com/tofupilot/cli
cd cli
cargo build --release   # binary at target/release/tofupilot
```

## Quick start

```bash
# Authenticate (opens a browser)
tofupilot login

# Pull a procedure
tofupilot pull <procedure-id>

# Run it
tofupilot run <procedure-id>
```

Run `tofupilot --help` for the full command surface.

## Configuration

The CLI stores credentials in `~/.tofupilot/credentials.json`. The default server is `https://www.tofupilot.app`; override with `--url` on `login`.

## Status

Pre-1.0. Command surface may change.

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md) for build, test, and lint instructions,
and [ARCHITECTURE.md](./ARCHITECTURE.md) for a tour of the codebase. Security
issues: [SECURITY.md](./SECURITY.md). Please follow our
[Code of Conduct](./CODE_OF_CONDUCT.md).

> This repository is published from the TofuPilot monorepo. Pull requests are
> welcome; changes are merged upstream and mirrored back here.

## License

[MIT](./LICENSE)
