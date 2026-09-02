//! Shared fixtures for the crate's unit tests.

use crate::job::Job;
use crate::procedure::schema::StageScope;

/// A bare job in the given stage: placeholder module/function, no deps,
/// no plugs, no UI, no timeout, no retry, no measurements.
pub(crate) fn job(scope: StageScope) -> Job {
    Job::new(
        None,
        "k".into(),
        "Phase".into(),
        scope,
        "m".into(),
        "f".into(),
        vec![],
        vec![],
        crate::ui::types::UiConfig::default(),
        None,
        None,
        None,
        &std::collections::HashMap::new(),
        vec![],
    )
}
