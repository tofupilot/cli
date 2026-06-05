//! Resolves which deployment a run originated from, for run attribution.

/// Resolve the deployment_id this run came from, by reading the local
/// `PullState` for the procedure. Returns `None` when the procedure
/// wasn't pulled from a deployment (ad-hoc / local path runs) or the
/// state DB is unavailable — the run is then created without a
/// deployment link. Errors that prevent the lookup (db open, read) are
/// logged as warnings rather than propagated: a metadata lookup failure
/// must not block the run, but the unlinked run should be debuggable.
pub fn lookup_deployment_id(procedure_id: &str) -> Option<String> {
    let db = match crate::commands::db::open() {
        Ok(db) => db,
        Err(e) => {
            crate::log::warn(&format!(
                "deployment_id lookup: state db unavailable ({e}); run will not be linked to a deployment"
            ));
            return None;
        }
    };
    match db.get_pull_state(procedure_id) {
        Ok(state) => state.map(|s| s.deployment_id),
        Err(e) => {
            crate::log::warn(&format!(
                "deployment_id lookup: failed to read pull state for {procedure_id} ({e}); run will not be linked to a deployment"
            ));
            None
        }
    }
}
