#![allow(dead_code)]

use std::sync::OnceLock;
use tofupilot::TofuPilot;
use tofupilot::config::ClientConfig;
use tofupilot::types::*;

static CLIENT: OnceLock<TofuPilot> = OnceLock::new();
static PROCEDURE_ID: OnceLock<String> = OnceLock::new();

pub fn uid() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn load_env() {
    // Load shared clients/.env.local
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let env_path = manifest.parent().unwrap().join(".env.local");
    if let Ok(contents) = std::fs::read_to_string(&env_path) {
        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') { continue; }
            if let Some(idx) = trimmed.find('=') {
                let key = &trimmed[..idx];
                let val = &trimmed[idx + 1..];
                if std::env::var(key).is_err() {
                    std::env::set_var(key, val);
                }
            }
        }
    }
}

pub fn client() -> &'static TofuPilot {
    CLIENT.get_or_init(|| {
        load_env();
        let api_key = std::env::var("TOFUPILOT_API_KEY_USER")
            .expect("TOFUPILOT_API_KEY_USER must be set — check clients/.env.local");
        let url = std::env::var("TOFUPILOT_URL")
            .unwrap_or_else(|_| "http://localhost:3000".to_string());

        TofuPilot::with_config(
            ClientConfig::new(api_key)
                .base_url(format!("{}/api", url)),
        )
    })
}

pub async fn procedure_id() -> &'static str {
    if let Some(id) = PROCEDURE_ID.get() {
        return id;
    }

    let c = client();
    let proc = c.procedures().create()
        .name(format!("Rust Test {}", uid()))
        .send()
        .await
        .expect("failed to create test procedure");

    PROCEDURE_ID.get_or_init(|| proc.id)
}

pub async fn create_test_run(uid_val: &str) -> RunCreateResponse {
    let c = client();
    let now = chrono::Utc::now();
    let proc_id = procedure_id().await;

    c.runs().create()
        .serial_number(format!("SN-{uid_val}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-{uid_val}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(Outcome::Pass)
        .send()
        .await
        .expect("failed to create test run")
}
