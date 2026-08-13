#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use tofupilot::config::ClientConfig;
use tofupilot::types::*;
use tofupilot::TofuPilot;

static CLIENT: OnceLock<TofuPilot> = OnceLock::new();
static PROCEDURE_ID: OnceLock<String> = OnceLock::new();
static TAG: OnceLock<String> = OnceLock::new();
static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Marks every name this run creates as belonging to this CI run.
///
/// The suites share one org, so clients/e2e-cleanup.py deletes by tag rather
/// than by age: untagged entities are never touched, which puts a concurrent
/// job's data and anything a human seeded out of reach by construction.
pub fn tag() -> &'static str {
    TAG.get_or_init(|| {
        load_env();
        std::env::var("E2E_TAG").unwrap_or_else(|_| {
            // Local runs need a tag of their own: with a counter, a fixed tag
            // would make two runs in a row produce exactly the same names.
            // Ten characters, like CI's, so a fragment costs the same either way.
            format!("e2el{}", &uuid::Uuid::new_v4().simple().to_string()[..6])
        })
    })
}

/// A name fragment no other run — and no other name in this run — produces.
///
/// The "r" marks the rust suite; python, C++ and C# use "p", "c" and "s".
/// See clients/python-speakeasy/tests/e2e_tag.py for why uniqueness inside a
/// run is a counter and not a uuid, and for the 60-character budget the four
/// suites share.
pub fn uid() -> String {
    format!(
        "{}r{}",
        tag(),
        base36(COUNTER.fetch_add(1, Ordering::Relaxed))
    )
}

/// Widens past three characters rather than wrapping: past 46 656 names a
/// fragment grows, which is visible, instead of repeating one, which is not.
fn base36(mut n: usize) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[n % 36]);
        n /= 36;
    }
    while out.len() < 3 {
        out.push(b'0');
    }
    out.reverse();
    String::from_utf8(out).expect("base36 digits are ascii")
}

fn load_env() {
    // Load shared clients/.env.local
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let env_path = manifest.parent().unwrap().join(".env.local");
    if let Ok(contents) = std::fs::read_to_string(&env_path) {
        for line in contents.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
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
        let url =
            std::env::var("TOFUPILOT_URL").unwrap_or_else(|_| "http://localhost:3000".to_string());

        TofuPilot::with_config(ClientConfig::new(api_key).base_url(format!("{}/api", url)))
    })
}

pub async fn procedure_id() -> &'static str {
    if let Some(id) = PROCEDURE_ID.get() {
        return id;
    }

    let c = client();
    let proc = c
        .procedures()
        .create()
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

    c.runs()
        .create()
        .serial_number(format!("SN-{uid_val}"))
        .procedure_id(proc_id)
        .part_number(format!("PART-{uid_val}"))
        .started_at(now - chrono::Duration::minutes(5))
        .ended_at(now)
        .outcome(LogGetOutcome::Pass)
        .send()
        .await
        .expect("failed to create test run")
}
