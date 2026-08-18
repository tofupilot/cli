//! Recently opened Studio project roots, persisted per machine.
//!
//! Two callers, one file:
//!   * `tofupilot studio` with no usable path reopens the most recent
//!     root that still exists, VSCode-style.
//!   * the studio session records every root the operator opens, so
//!     the dashboard's project switcher has something to list.
//!
//! Persisted on the DAEMON side on purpose. The browser must never be
//! the authority on which paths are legitimate: a root reaches this
//! file only after a human designated it (launch argument, or the OS
//! folder dialog), and `open_project` may then only select among the
//! entries here. Storing it browser-side would hand that authority to
//! whatever holds the session token.
//!
//! Every read tolerates a missing or corrupt file by returning an empty
//! list: a mangled recents file is a convenience lost, never a reason
//! to refuse to start.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Cap on the stored list. The switcher is a menu, not an archive, and
/// an unbounded list is a slow leak nobody ever notices.
const MAX_RECENTS: usize = 10;

#[derive(Serialize, Deserialize, Default)]
struct RecentsFile {
    /// Canonical roots, most recently opened first.
    roots: Vec<PathBuf>,
}

/// Where the list lives. Shares the CLI's state directory with the
/// credentials slots; falls back the same way they do so a read-only
/// home degrades to a no-op instead of a panic.
pub fn recents_path() -> PathBuf {
    super::db::tofupilot_dir()
        .unwrap_or_else(|_| PathBuf::from(".tofupilot"))
        .join("studio-recents.json")
}

/// Stored roots, most recent first. Absent file, unreadable file and
/// malformed JSON all read as "no history".
pub fn load_from(file: &Path) -> Vec<PathBuf> {
    let Ok(bytes) = std::fs::read(file) else {
        return Vec::new();
    };
    serde_json::from_slice::<RecentsFile>(&bytes)
        .map(|f| f.roots)
        .unwrap_or_default()
}

/// Stored roots that still exist on disk. A project folder can be
/// moved, renamed or deleted between two sessions, and offering to
/// reopen a path that is gone is worse than not offering it.
pub fn existing_from(file: &Path) -> Vec<PathBuf> {
    load_from(file).into_iter().filter(|p| p.is_dir()).collect()
}

/// Promote `root` to the head of the list and persist it.
///
/// `root` must already be canonical: the whole point of the list is
/// that a path in it has been vetted once, and two spellings of one
/// directory would both defeat the dedupe and inflate the cap.
pub fn record_in(file: &Path, root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut roots = load_from(file);
    // Dedupe on the way in rather than on read: re-opening a project
    // must move it up, not add a second entry that pushes a genuinely
    // older one past the cap.
    roots.retain(|p| p != root);
    roots.insert(0, root.to_path_buf());
    roots.truncate(MAX_RECENTS);

    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(&RecentsFile {
        roots: roots.clone(),
    })?;
    std::fs::write(file, body)?;
    Ok(roots)
}

/// `existing_from` against the default path.
pub fn existing() -> Vec<PathBuf> {
    existing_from(&recents_path())
}

/// `record_in`, with failures swallowed: not being able to remember a
/// project must not fail opening it.
pub fn record_in_or_warn(file: &Path, root: &Path) -> Vec<PathBuf> {
    record_in(file, root).unwrap_or_else(|e| {
        crate::log::warn(&format!(
            "could not record the recent project list ({e}); the switcher will forget this project"
        ));
        load_from(file)
    })
}

/// `record_in_or_warn` against the default path.
pub fn record(root: &Path) -> Vec<PathBuf> {
    record_in_or_warn(&recents_path(), root)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// tempfile-backed fixture, same idiom as local_ws/studio.rs: a
    /// hand-rolled Drop keyed on process::id() was identical for every
    /// test in the binary and leaked its directory on a killed run.
    struct TempDir(tempfile::TempDir);

    impl TempDir {
        fn new(_tag: &str) -> Self {
            Self(tempfile::tempdir().expect("temp dir"))
        }
        fn file(&self) -> PathBuf {
            self.0.path().join("studio-recents.json")
        }
        fn dir(&self, name: &str) -> PathBuf {
            let p = self.0.path().join(name);
            std::fs::create_dir_all(&p).expect("project dir");
            p
        }
    }

    #[test]
    fn missing_file_reads_as_no_history() {
        let tmp = TempDir::new("missing");
        assert!(load_from(&tmp.file()).is_empty());
        assert!(existing_from(&tmp.file()).is_empty());
    }

    #[test]
    fn corrupt_file_reads_as_no_history_instead_of_failing() {
        let tmp = TempDir::new("corrupt");
        std::fs::write(tmp.file(), b"{ this is not json").unwrap();
        assert!(load_from(&tmp.file()).is_empty());
    }

    #[test]
    fn round_trips_a_recorded_root() {
        let tmp = TempDir::new("roundtrip");
        let project = tmp.dir("alpha");
        record_in(&tmp.file(), &project).unwrap();
        assert_eq!(load_from(&tmp.file()), vec![project]);
    }

    #[test]
    fn most_recently_recorded_comes_first() {
        let tmp = TempDir::new("order");
        let (a, b, c) = (tmp.dir("a"), tmp.dir("b"), tmp.dir("c"));
        record_in(&tmp.file(), &a).unwrap();
        record_in(&tmp.file(), &b).unwrap();
        record_in(&tmp.file(), &c).unwrap();
        assert_eq!(load_from(&tmp.file()), vec![c, b, a]);
    }

    #[test]
    fn reopening_a_known_root_moves_it_up_without_duplicating() {
        let tmp = TempDir::new("dedupe");
        let (a, b) = (tmp.dir("a"), tmp.dir("b"));
        record_in(&tmp.file(), &a).unwrap();
        record_in(&tmp.file(), &b).unwrap();
        let after = record_in(&tmp.file(), &a).unwrap();
        assert_eq!(after, vec![a, b], "re-opened root should be promoted once");
    }

    #[test]
    fn list_is_capped_and_drops_the_oldest() {
        let tmp = TempDir::new("cap");
        let dirs: Vec<PathBuf> = (0..MAX_RECENTS + 3)
            .map(|i| tmp.dir(&format!("p{i}")))
            .collect();
        for d in &dirs {
            record_in(&tmp.file(), d).unwrap();
        }
        let stored = load_from(&tmp.file());
        assert_eq!(stored.len(), MAX_RECENTS);
        assert_eq!(stored[0], dirs[dirs.len() - 1], "newest stays at the head");
        assert!(
            !stored.contains(&dirs[0]),
            "the oldest entry should have fallen off the end"
        );
    }

    #[test]
    fn vanished_projects_are_kept_in_the_file_but_not_offered() {
        let tmp = TempDir::new("vanished");
        let (alive, gone) = (tmp.dir("alive"), tmp.dir("gone"));
        record_in(&tmp.file(), &alive).unwrap();
        record_in(&tmp.file(), &gone).unwrap();
        std::fs::remove_dir_all(&gone).unwrap();

        // Raw history is unchanged — the file is not rewritten behind
        // the user's back just because a disk changed.
        assert_eq!(load_from(&tmp.file()).len(), 2);
        // What gets offered is only what can actually be opened.
        assert_eq!(existing_from(&tmp.file()), vec![alive]);
    }

    #[test]
    fn recording_creates_the_state_directory_when_absent() {
        let tmp = TempDir::new("mkdir");
        let nested = tmp.0.path().join("does").join("not").join("exist");
        let file = nested.join("studio-recents.json");
        let project = tmp.dir("alpha");
        record_in(&file, &project).unwrap();
        assert_eq!(load_from(&file), vec![project]);
    }
}
