# station-protocol

Shared protocol types exchanged between the [TofuPilot CLI](https://github.com/tofupilot/cli), the [execution engine](https://github.com/tofupilot/framework), and the TofuPilot dashboard.

This crate defines the wire format for real-time station <-> dashboard messaging (over Centrifugo channels): events the station publishes (`StationEvent`), commands the dashboard sends (`StationCommand`), and the UI component vocabulary used to render operator prompts.

## Scope

Workspace-internal crate, not published to crates.io. Re-exported by `execution-engine` for downstream consumers.

A binary, `export-types`, generates the TypeScript bindings the dashboard consumes:

```bash
cargo run --bin export-types
```

## License

MIT
