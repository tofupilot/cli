//! Helper for `tests/plug_orphan.rs`: spawns one plug service through
//! the real `PlugServiceManager` path, reports its port, then waits to
//! be SIGKILLed. The test needs a parent process that is not itself.
//!
//! Args: `<procedure-dir> <python> <plug-file>`.

use std::sync::Arc;

use execution_engine::plugs::plug_service::PlugServiceManager;
use execution_engine::{EventSink, NullSink};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let dir = std::path::PathBuf::from(args.next().expect("procedure dir"));
    let python = std::path::PathBuf::from(args.next().expect("python path"));
    let plug_file = args.next().expect("plug file");

    let manager = PlugServiceManager::new_with_python(dir, Some(python));
    let sink: Arc<dyn EventSink> = Arc::new(NullSink);
    let port = manager
        .start_plug_service(
            "psu".to_string(),
            "psu".to_string(),
            "PSU".to_string(),
            serde_json::json!({ "file": plug_file, "class": "Plug", "config": {} }),
            None,
            &sink,
        )
        .await
        .expect("plug service starts");
    println!("PORT:{port}");

    // The manager, and with it the child handle, stays alive until the
    // test kills this process: no Drop, no Shutdown RPC, the orphan case.
    std::future::pending::<()>().await;
    drop(manager);
}
