//! On-demand `uv` bootstrap.
//!
//! `uv` is required for procedure venv creation and dep sync, and we
//! can't assume the host has it on PATH. `ensure_uv()` resolves a
//! usable uv path so `tofupilot run` works on a fresh machine without
//! a prior install step:
//! 1. PATH lookup — honour any system-installed uv (homebrew, apt,
//!    user-managed cargo install).
//! 2. Local cache at `~/.tofupilot/bin/uv` — populated on first call.
//! 3. Fresh download from the TofuPilot R2 mirror into the cache.
//!
//! Silent: no operator prompt. The download surfaces progress and
//! errors via stderr; on failure the caller sees a clear "couldn't
//! provision uv" message instead of an opaque ENOENT.

use std::path::{Path, PathBuf};

use crate::commands::db::tofupilot_dir;

#[cfg(windows)]
const UV_BINARY: &str = "uv.exe";
#[cfg(not(windows))]
const UV_BINARY: &str = "uv";

/// Archive format published by astral-sh for a given target.
enum ArchiveKind {
    TarGz,
    Zip,
}

/// Pinned `uv` release. Bump deliberately — stations across a fleet
/// must agree on the uv version so a `pull` on one host produces an
/// identical venv to a `pull` on another. Following `latest` would
/// silently drift between machines and break that invariant.
///
/// A bump must ship in a CLI release: the "Mirror uv to R2" step in
/// release-cli.yml parses this constant (and the sha256 pins below)
/// and uploads the new archives to the mirror. Until that release
/// runs, the new version does not exist at `UV_BASE_URL`.
const UV_VERSION: &str = "0.11.8";

/// Default base URL for uv archives. The pinned release is mirrored
/// from astral-sh GitHub releases into the same R2 bucket that serves
/// CLI binaries (dl.tofupilot.sh) — github.com is unreachable from
/// China, so like the CLI itself, uv must not depend on it at install
/// time. Override with `TOFUPILOT_UV_BASE` for self-hosted or
/// air-gapped mirrors; the layout is `{base}/{UV_VERSION}/uv-{target}.{ext}`.
const UV_BASE_URL: &str = "https://dl.tofupilot.sh/uv";

/// Resolve a usable `uv` executable. Returns the path to invoke;
/// callers should pass it to `Command::new` instead of a bare "uv".
///
/// A cached uv whose version doesn't match `UV_VERSION` is re-fetched
/// — the CLI bumps `UV_VERSION` deliberately and stations must
/// converge on the new one as soon as the new CLI runs.
pub async fn ensure_uv() -> crate::error::CliResult<PathBuf> {
    if let Some(path) = find_on_path() {
        return Ok(path);
    }

    let cache = cached_path()?;
    if cache.exists() && cached_version_matches(&cache) {
        return Ok(cache);
    }

    crate::log::info(&format!("Provisioning uv {UV_VERSION}..."));
    download_uv(&cache).await?;
    Ok(cache)
}

/// Probe the cached binary by running `uv --version`. Returns true
/// if the output matches `UV_VERSION`. Failures (binary corrupt,
/// missing exec bit, version mismatch) all collapse to false so
/// the caller re-downloads.
fn cached_version_matches(path: &Path) -> bool {
    let Ok(output) = std::process::Command::new(path).arg("--version").output() else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    // `uv --version` prints `uv X.Y.Z` plus a trailing newline.
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .split_whitespace()
        .nth(1)
        .is_some_and(|v| v == UV_VERSION)
}

/// Find a `uv` on PATH whose version matches `UV_VERSION`. A
/// system-installed uv is only honoured when it agrees with the pinned
/// version: otherwise a stale homebrew/system uv would shadow the
/// pinned cache binary forever, breaking the fleet-consensus invariant
/// (stations must build identical venvs) and re-introducing the
/// "uv too old to fetch this Python build" failure that the pin exists
/// to prevent. A non-matching PATH uv is skipped, not used.
fn find_on_path() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(UV_BINARY);
        if candidate.is_file() && cached_version_matches(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn cached_path() -> crate::error::CliResult<PathBuf> {
    Ok(tofupilot_dir()?.join("bin").join(UV_BINARY))
}

/// Map the Rust `target_os` + `target_arch` to the uv release filename
/// astral-sh publishes plus its pinned sha256. Returns the archive
/// kind too — uv tarballs nest the executable in a per-target
/// directory, so we need to know how to extract.
///
/// **sha256 pinning** — bumping `UV_VERSION` requires regenerating
/// the hashes below from astral-sh's `.sha256` sidecars. Pinning in
/// source defends against a coordinated supply-chain swap (mirror +
/// sidecar tampered together) that wire-side sha verification can't
/// catch — including a compromise of our own R2 mirror. The
/// "Mirror uv to R2" step in release-cli.yml greps these pins (the
/// quoted triple followed by a 64-hex literal) to verify archives
/// before upload; keep that literal layout. Regenerate with:
/// ```sh
/// for t in aarch64-apple-darwin x86_64-apple-darwin \
///          aarch64-unknown-linux-musl x86_64-unknown-linux-musl \
///          x86_64-pc-windows-msvc aarch64-pc-windows-msvc; do
///   ext=$([[ "$t" == *windows* ]] && echo zip || echo tar.gz)
///   curl -fsSL "https://github.com/astral-sh/uv/releases/download/$UV_VERSION/uv-${t}.${ext}.sha256" | awk '{print $1}'
/// done
/// ```
fn release_target() -> crate::error::CliResult<(&'static str, ArchiveKind, &'static str)> {
    let triple = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => (
            "aarch64-apple-darwin",
            ArchiveKind::TarGz,
            "c729adb365114e844dd7f9316313a7ed6443b89bb5681d409eebac78b0bd06c8",
        ),
        ("macos", "x86_64") => (
            "x86_64-apple-darwin",
            ArchiveKind::TarGz,
            "c59d73bf34b58bc8e33a11629f7a255c11789fd00f03cd3e68ab2d1603645de9",
        ),
        ("linux", "aarch64") => (
            "aarch64-unknown-linux-musl",
            ArchiveKind::TarGz,
            "29418befb64f926a2dba3473e8e69acd00b36fb845d85344ef11321a993ad8f5",
        ),
        ("linux", "x86_64") => (
            "x86_64-unknown-linux-musl",
            ArchiveKind::TarGz,
            "de82507d12e31cfc86c1c776238f7c248e48e40d996dedc812d64fdd31c6ed12",
        ),
        ("windows", "x86_64") => (
            "x86_64-pc-windows-msvc",
            ArchiveKind::Zip,
            "c84629a56e0706b69a47ea35862208af827cb6fbfa1d0ca763c52c67594637e8",
        ),
        ("windows", "aarch64") => (
            "aarch64-pc-windows-msvc",
            ArchiveKind::Zip,
            "bb48716e74e4998993f15bc57a55e4d0d73ccbd27a66d7cbed37605f7c67d747",
        ),
        (os, arch) => {
            return Err(format!(
                "uv auto-install unsupported on {os}/{arch}. Install uv manually \
                 (https://docs.astral.sh/uv/getting-started/installation/) and \
                 rerun."
            )
            .into());
        }
    };
    Ok(triple)
}

async fn download_uv(dest: &Path) -> crate::error::CliResult<()> {
    let (target, kind, expected_sha) = release_target()?;
    let ext = match kind {
        ArchiveKind::TarGz => "tar.gz",
        ArchiveKind::Zip => "zip",
    };
    let base = std::env::var("TOFUPILOT_UV_BASE")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| UV_BASE_URL.to_string());
    let url = format!(
        "{}/{UV_VERSION}/uv-{target}.{ext}",
        base.trim_end_matches('/')
    );

    let bytes = crate::http::client()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Download uv: {e}"))?
        .error_for_status()
        .map_err(|e| format!("Download uv: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("Download uv: {e}"))?;

    // sha256 pinned in source (see `release_target`). Fetching from a
    // `.sha256` sidecar on the same release URL only defends against
    // in-flight corruption — a hostile proxy / mirror swapping the
    // archive could swap the sidecar in lockstep. Pinning means a
    // tampered binary is rejected even if the entire release page is
    // compromised.
    let actual_sha = sha256_hex(&bytes);
    if !actual_sha.eq_ignore_ascii_case(expected_sha) {
        return Err(format!(
            "uv archive sha256 mismatch: expected {expected_sha}, got {actual_sha}"
        )
        .into());
    }

    let parent = dest
        .parent()
        .ok_or_else(|| "Cache path has no parent".to_string())?;
    std::fs::create_dir_all(parent).map_err(|e| format!("Create {}: {e}", parent.display()))?;

    match kind {
        ArchiveKind::TarGz => extract_uv_binary_tar(&bytes, dest)?,
        ArchiveKind::Zip => extract_uv_binary_zip(&bytes, dest)?,
    }

    // Atomic permissions: 0o755 so the binary is executable for the
    // current user. `set_permissions` overwrites mode wholesale.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod {}: {e}", dest.display()))?;
    }

    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for byte in out.iter() {
        use std::fmt::Write;
        let _ = write!(&mut s, "{:02x}", byte);
    }
    s
}

/// Extract just the `uv` executable from the release tarball. The
/// archive has the layout `uv-{target}/uv` (plus `uvx` and a
/// LICENSE). We only need `uv`; ignore everything else.
fn extract_uv_binary_tar(archive_bytes: &[u8], dest: &Path) -> crate::error::CliResult<()> {
    let gz = flate2::read::GzDecoder::new(archive_bytes);
    let mut tar = tar::Archive::new(gz);

    for entry in tar.entries().map_err(|e| format!("Read uv archive: {e}"))? {
        let mut entry = entry.map_err(|e| format!("Read uv archive entry: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("Read uv archive entry path: {e}"))?
            .into_owned();
        if path.file_name().and_then(|s| s.to_str()) == Some(UV_BINARY) {
            // Unpack to a temp file in the same dir, then rename — so
            // a partial extract doesn't leave a half-written binary
            // at `dest` that future calls would short-circuit on.
            let tmp = dest.with_extension("partial");
            entry.unpack(&tmp).map_err(|e| format!("Unpack uv: {e}"))?;
            std::fs::rename(&tmp, dest).map_err(|e| format!("Move uv into place: {e}"))?;
            return Ok(());
        }
    }
    Err(format!("'{UV_BINARY}' entry not found in release tarball").into())
}

/// Extract `uv.exe` from the Windows release zip. Layout is flat:
/// `uv.exe`, `uvx.exe` at the archive root.
fn extract_uv_binary_zip(archive_bytes: &[u8], dest: &Path) -> crate::error::CliResult<()> {
    let reader = std::io::Cursor::new(archive_bytes);
    let mut archive = zip::ZipArchive::new(reader).map_err(|e| format!("Open uv zip: {e}"))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("Read uv zip entry: {e}"))?;
        let name = entry.name().to_string();
        if std::path::Path::new(&name)
            .file_name()
            .and_then(|s| s.to_str())
            == Some(UV_BINARY)
        {
            let tmp = dest.with_extension("partial");
            let mut out = std::fs::File::create(&tmp)
                .map_err(|e| format!("Create {}: {e}", tmp.display()))?;
            std::io::copy(&mut entry, &mut out).map_err(|e| format!("Unpack uv: {e}"))?;
            drop(out);
            std::fs::rename(&tmp, dest).map_err(|e| format!("Move uv into place: {e}"))?;
            return Ok(());
        }
    }
    Err(format!("'{UV_BINARY}' entry not found in release zip").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hits the network: downloads the pinned uv release and verifies
    /// `uv --version` matches `UV_VERSION`. `#[ignore]` because cargo
    /// test runs offline by default; invoke with
    /// `cargo test -- --ignored test_ensure_uv_e2e`.
    #[tokio::test]
    #[ignore]
    async fn test_ensure_uv_e2e() {
        // Use a temp dir so the test doesn't pollute ~/.tofupilot/bin.
        let tmp = std::env::temp_dir().join(format!("uv_test_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let dest = tmp.join("uv");

        download_uv(&dest).await.expect("download_uv failed");
        assert!(dest.is_file(), "uv binary not written");

        let out = std::process::Command::new(&dest)
            .arg("--version")
            .output()
            .expect("uv --version failed");
        assert!(out.status.success());
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(UV_VERSION),
            "uv --version output {stdout:?} does not contain {UV_VERSION}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
