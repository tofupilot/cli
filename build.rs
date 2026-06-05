use std::fs;
use std::path::PathBuf;

fn main() {
    // The local-UI server embeds the operator-ui Vite build via
    // `include_dir!`. The macro fails the build if its target path
    // doesn't exist, so guarantee the directory is present (empty is
    // fine — the server falls back to a "build the SPA first" page).
    let dist = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("operator-ui/dist");
    if !dist.exists() {
        let _ = fs::create_dir_all(&dist);
    }
    // Re-run if the SPA artifacts change so iterative dev sees fresh
    // bundles without a full clean.
    println!("cargo:rerun-if-changed={}", dist.display());
}
