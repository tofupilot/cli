# TofuPilot Rust SDK

Rust SDK for the [TofuPilot](https://tofupilot.com) REST API. Async, typed, with retries and request lifecycle hooks.

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
tofupilot = "0.1"
tokio = { version = "1", features = ["full"] }
```

## Quick Start

```rust,no_run
use tofupilot::TofuPilot;
use tofupilot::types::*;

#[tokio::main]
async fn main() -> tofupilot::Result<()> {
    let client = TofuPilot::new("your-api-key");

    // Create a test run
    let run = client.runs().create()
        .procedure_id("550e8400-e29b-41d4-a716-446655440000")
        .serial_number("SN-001234")
        .part_number("PCB-V1.2")
        .outcome(Outcome::Pass)
        .started_at(chrono::Utc::now() - chrono::TimeDelta::minutes(5))
        .ended_at(chrono::Utc::now())
        .send()
        .await?;

    println!("Created run: {}", run.id);
    Ok(())
}
```

## Available Resources

| Resource | Methods | Docs |
| --- | --- | --- |
| **Runs** | list, create, get, update, delete | [docs/sdks/runs](https://github.com/tofupilot/rust/blob/main/docs/sdks/runs/README.md) |
| **Runs.Attachments** | upload, download | - |
| **Procedures** | list, create, get, update, delete | [docs/sdks/procedures](https://github.com/tofupilot/rust/blob/main/docs/sdks/procedures/README.md) |
| **Units** | list, create, get, update, delete, add_child, remove_child | [docs/sdks/units](https://github.com/tofupilot/rust/blob/main/docs/sdks/units/README.md) |
| **Units.Attachments** | upload, download, delete | - |
| **Parts** | list, create, get, update, delete | [docs/sdks/parts](https://github.com/tofupilot/rust/blob/main/docs/sdks/parts/README.md) |
| **Batches** | list, create, get, update, delete | [docs/sdks/batches](https://github.com/tofupilot/rust/blob/main/docs/sdks/batches/README.md) |
| **Stations** | list, create, get, get_current, update, remove | [docs/sdks/stations](https://github.com/tofupilot/rust/blob/main/docs/sdks/stations/README.md) |
| **Revisions** | create, get, update, delete | [docs/sdks/revisions](https://github.com/tofupilot/rust/blob/main/docs/sdks/revisions/README.md) |
| **Versions** | create, get, delete | [docs/sdks/versions](https://github.com/tofupilot/rust/blob/main/docs/sdks/versions/README.md) |
| **User** | list | [docs/sdks/user](https://github.com/tofupilot/rust/blob/main/docs/sdks/user/README.md) |

All model types are documented in [docs/models/](https://github.com/tofupilot/rust/tree/main/docs/models).

## Builder Pattern

Every API call uses the builder pattern. Required fields are enforced at send time:

```rust,ignore
// Optional fields are chained before .send()
let runs = client.runs().list()
    .outcomes(vec![Outcome::Pass])
    .part_numbers(vec!["PCB-V1.2".into()])
    .limit(50)
    .sort_by(RunListSortBy::StartedAt)
    .sort_order(ListSortOrder::Desc)
    .send()
    .await?;

for run in &runs.data {
    println!("{}: {:?}", run.id, run.outcome);
}
```

## File Attachments

Attach files directly to runs or units:

```rust,ignore
// Upload a file to a run
let id = client.runs().attachments().upload(&run.id, "report.pdf").await?;

// Upload a file to a unit
let id = client.units().attachments().upload("SN-0001", "calibration.pdf").await?;

// Download an attachment
client.runs().attachments().download(&url, "local-report.pdf").await?;

// Delete a unit attachment
client.units().attachments().delete("SN-0001", vec![id]).await?;
```

## Phases & Measurements

```rust,ignore
use tofupilot::types::*;

let now = chrono::Utc::now();

let run = client.runs().create()
    .procedure_id(proc_id)
    .serial_number("SN-001")
    .part_number("PCB-V1")
    .outcome(Outcome::Pass)
    .started_at(now - chrono::TimeDelta::minutes(5))
    .ended_at(now)
    .phases(vec![RunCreatePhases::builder()
        .name("voltage_check")
        .outcome(PhasesOutcome::Pass)
        .started_at(now - chrono::TimeDelta::minutes(5))
        .ended_at(now - chrono::TimeDelta::minutes(3))
        .measurements(vec![RunCreateMeasurements::builder()
            .name("output_voltage")
            .outcome(ValidatorsOutcome::Pass)
            .measured_value(3.3)
            .units("V")
            .build()
            .unwrap()
        ])
        .build()
        .unwrap()
    ])
    .send()
    .await?;
```

## Error Handling

All API errors are typed:

```rust,ignore
use tofupilot::Error;

match client.runs().get().id("nonexistent").send().await {
    Ok(run) => println!("Found: {}", run.id),
    Err(Error::NotFound(e)) => println!("Not found: {}", e.message),
    Err(Error::Unauthorized(e)) => println!("Bad API key: {}", e.message),
    Err(Error::BadRequest(e)) => {
        println!("Validation error: {}", e.message);
        for issue in &e.issues {
            println!("  - {}", issue.message);
        }
    }
    Err(e) => println!("Other error: {e}"),
}
```

## Retries

The client automatically retries on 429 (rate limit) and 5xx errors with exponential backoff. Configure via `ClientConfig`:

```rust,ignore
use tofupilot::config::ClientConfig;
use std::time::Duration;

let client = TofuPilot::with_config(
    ClientConfig::new("your-api-key")
        .base_url("https://your-instance.tofupilot.app/api")
        .timeout(Duration::from_secs(60))
        .max_retries(5),
);
```

## Hooks

Inspect or modify requests and responses:

```rust,ignore
use tofupilot::Hooks;

let hooks = Hooks::new()
    .on_before_request(|ctx, req| async move {
        println!("[{}] {} {}", ctx.operation_id, req.method(), req.url());
        req
    })
    .on_after_error(|ctx, err| {
        let msg = format!("[{}] Error: {err}", ctx.operation_id);
        async move { eprintln!("{msg}"); }
    });

let client = TofuPilot::with_config(
    ClientConfig::new("your-api-key").hooks(hooks),
);
```

## Nullable Fields

Some fields distinguish between "not sent" and "explicitly null". These use `NullableField<T>`:

```rust,ignore
use tofupilot::types::NullableField;

// Has a value (From<T> impl)
let field: NullableField<String> = "hello".to_string().into();

// Convenience constructors
let field = NullableField::value("hello".to_string());
let field: NullableField<String> = NullableField::null();
```

Builder methods handle this automatically — you never need to construct `NullableField` manually:

```rust,ignore
client.runs().create()
    .procedure_version("1.2.3")    // sets Value("1.2.3")
    .procedure_version_null()       // sets Null
    // omitted fields default to Absent
```

## Self-Hosted

Point the client at your own TofuPilot instance:

```rust,ignore
let client = TofuPilot::with_config(
    ClientConfig::new("your-api-key")
        .base_url("https://your-instance.example.com/api"),
);
```

## Per-Request Overrides

Override server URL or timeout for individual requests:

```rust,ignore
let result = client.runs().list()
    .server_url("https://staging.tofupilot.app/api")
    .timeout(std::time::Duration::from_secs(120))
    .send()
    .await?;
```

## Documentation

- [Getting Started](https://tofupilot.com/docs/getting-started-with-tofupilot)
- [Rust SDK Guide](https://tofupilot.com/docs/sdk/rust)
- [API Reference](https://tofupilot.com/docs/api/rest-api)
- [Changelog](https://tofupilot.com/changelog)
