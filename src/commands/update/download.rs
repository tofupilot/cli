//! Downloads and stages a new binary: streams the release archive, verifies its
//! checksum, and extracts the binary to a staged path.

use flate2::read::GzDecoder;
use futures::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::{fs, path::Path};
use tar::Archive;

use super::cache;
use super::config::{DOWNLOAD_TIMEOUT, REQUEST_TIMEOUT, VERSION_URL};

use super::platform::platform_key;

#[derive(Debug, Deserialize)]
pub struct VersionInfo {
    pub latest: String,
    pub min: Option<String>,
    pub urls: HashMap<String, String>,
    pub checksums: Option<HashMap<String, String>>,
}

pub async fn fetch() -> crate::error::CliResult<VersionInfo> {
    let client = Client::builder().timeout(REQUEST_TIMEOUT).build()?;
    Ok(client
        .get(VERSION_URL)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

pub async fn download_and_stage(info: &VersionInfo, staged: &Path) -> crate::error::CliResult<()> {
    let key = platform_key();
    let url = info
        .urls
        .get(&key)
        .ok_or_else(|| format!("no download URL for {key}"))?;

    // Mandatory checksum. A missing checksum used to silently skip
    // verification — a partial/corrupted download would then exec into
    // SIGSEGV/SIGBUS when the new binary ran. Refuse to proceed.
    let expected_archive_sha = info
        .checksums
        .as_ref()
        .and_then(|c| c.get(&key))
        .ok_or_else(|| {
            format!(
                "server did not publish a checksum for {key}; refusing to stage unverified binary"
            )
        })?
        .clone();

    // Stream the archive to a sibling temp file so peak memory stays
    // small on RAM-constrained hosts (1G Raspberry Pi). Buffering the
    // whole compressed binary in RAM and decompressing into another
    // Vec<u8> peaks at ~150 MB; streaming caps at the chunk size
    // reqwest hands us.
    let archive_tmp = staged.with_extension("download");
    if let Some(parent) = archive_tmp.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::remove_file(&archive_tmp);

    let client = Client::builder().timeout(DOWNLOAD_TIMEOUT).build()?;
    let response = client.get(url).send().await?.error_for_status()?;
    let mut hasher = Sha256::new();
    {
        // Write-only handle for the streamed download; closed at the
        // end of this scope so the read-side reopen below sees a
        // fully-flushed file. `File::create` opens O_WRONLY, so we
        // can't reuse the same handle for reads — that's the EBADF
        // (os error 9) bug `tofupilot update` hits on Linux.
        let mut archive_file = File::create(&archive_tmp)?;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            hasher.update(&chunk);
            archive_file.write_all(&chunk)?;
        }
        archive_file.flush()?;
        // Force the archive contents to disk before we re-open it for
        // extraction. A power-cut or panic between write+open without
        // sync_all leaves zeroed tail blocks in the page cache that
        // hash fine but extract garbage.
        archive_file.sync_all()?;
    }

    let actual_archive_sha = hex::encode(hasher.finalize());
    if actual_archive_sha != expected_archive_sha {
        let _ = fs::remove_file(&archive_tmp);
        return Err("checksum verification failed".into());
    }

    // Extract to a sibling temp file, then atomically rename into
    // place. Writing directly to `staged` would leave a half-extracted
    // file at the canonical path if extraction died mid-copy — and
    // `apply_staged` would later exec it into a segfault.
    let staged_tmp = staged.with_extension("staged.tmp");
    let _ = fs::remove_file(&staged_tmp);

    let read_handle = File::open(&archive_tmp)?;
    let mut staged_file = File::create(&staged_tmp)?;
    let mut bin_hasher = Sha256::new();
    let extract_result = extract_binary_stream(read_handle, url, &mut staged_file, &mut bin_hasher);
    let _ = fs::remove_file(&archive_tmp);
    if let Err(e) = extract_result {
        drop(staged_file);
        let _ = fs::remove_file(&staged_tmp);
        return Err(e);
    }
    // Propagate flush error: a silent flush failure on a file we're
    // about to durably-sync would leave torn user-space buffers under
    // the eventual fsync, and `verify_staged` later would just say
    // "checksum mismatch" without explaining where the corruption
    // came from.
    staged_file.flush()?;
    // sync_all on the extracted binary before close. Same rationale as
    // the archive: the file we'll later mmap+exec must be durable.
    staged_file.sync_all()?;
    drop(staged_file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staged_tmp, fs::Permissions::from_mode(0o755))?;
    }

    // Persist the bin sha BEFORE the rename so a crash between
    // set_staged and rename leaves no observable staged file with no
    // recorded checksum. The temp file's bytes are identical to what
    // the rename will publish, so the recorded sha is correct
    // regardless of which side of the rename a later boot sees.
    let bin_sha = hex::encode(bin_hasher.finalize());
    cache::set_staged(&info.latest, &bin_sha)?;

    // Atomic publish: rename(2) on the same filesystem is atomic on
    // POSIX. On Windows std::fs::rename fails if the destination
    // exists, so remove first.
    #[cfg(windows)]
    {
        let _ = fs::remove_file(staged);
    }
    fs::rename(&staged_tmp, staged)?;
    // fsync the parent dir so the rename itself is durable.
    #[cfg(unix)]
    if let Some(parent) = staged.parent() {
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }

    Ok(())
}

/// Stream the binary out of the archive into `out`, copying chunk by
/// chunk so neither the compressed input nor the decompressed output
/// is ever fully resident in memory.
fn extract_binary_stream(
    archive: File,
    url: &str,
    out: &mut File,
    hasher: &mut Sha256,
) -> crate::error::CliResult<()> {
    if url.ends_with(".zip") {
        // `zip` needs random access on the underlying reader; a
        // BufReader<File> satisfies that without slurping the file.
        let mut zip =
            zip::ZipArchive::new(BufReader::new(archive)).map_err(crate::error::CliError::msg)?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).map_err(crate::error::CliError::msg)?;
            let name = std::path::Path::new(entry.name())
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            if name == "tofupilot" || name == "tofupilot.exe" {
                copy_in_chunks(&mut entry, out, hasher)?;
                return Ok(());
            }
        }
        return Err("binary not found in zip".into());
    }

    let mut tar = Archive::new(GzDecoder::new(BufReader::new(archive)));
    for entry in tar.entries()? {
        let mut entry = entry?;
        let name = entry
            .path()?
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if name == "tofupilot" || name == "tofupilot.exe" {
            copy_in_chunks(&mut entry, out, hasher)?;
            return Ok(());
        }
    }
    Err("binary not found in tarball".into())
}

fn copy_in_chunks<R: Read, W: Write>(
    src: &mut R,
    dst: &mut W,
    hasher: &mut Sha256,
) -> std::io::Result<()> {
    // 64 KB buffer: large enough to amortize syscall overhead, small
    // enough to keep peak memory negligible.
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        hasher.update(&buf[..n]);
        dst.write_all(&buf[..n])?;
    }
}
