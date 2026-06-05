# execution-engine

The execution engine powers [TofuPilot](https://tofupilot.com) test procedures: it loads a procedure definition, orchestrates its phases and plugs across workers, and emits structured events that the [TofuPilot CLI](https://github.com/tofupilot/cli) and dashboard render.

## What it does

- Parses procedure YAML (`procedure.yaml`) into a typed schema
- Schedules phases across slots with stage-aware concurrency
- Spawns Python, executable, and built-in runtime workers
- Streams structured events (`ExecutionEvent`) to any sink
- Handles operator UI requests (forms, prompts, identify) via a typed channel
- Coordinates graceful shutdown and per-job timeouts

## Quick start

```rust
use execution_engine::orchestrator::Orchestrator;
use execution_engine::procedure::loader::load_procedure_definition;

# async fn run() -> anyhow::Result<()> {
let procedure = load_procedure_definition("./procedure.yaml")?;
let orchestrator = Orchestrator::new(procedure, /* config */ Default::default());
orchestrator.run().await?;
# Ok(())
# }
```

See `examples/` for runnable samples.

## Status

Pre-1.0. The public API may change between minor versions. Pinned exact in the [CLI](https://github.com/tofupilot/cli).

## License

MIT
