use crate::async_jobs::JobManager;
use crate::context_compact::{JobLedgerItem, RuntimeLedgerSnapshot, SubagentLedgerItem};
use crate::subagent::SubagentStatus;

pub(crate) fn build_runtime_ledger_snapshot(session_id: &str) -> RuntimeLedgerSnapshot {
    let mut snapshot = RuntimeLedgerSnapshot::default();

    match JobManager::list_session_snapshots(session_id) {
        Ok(jobs) => {
            snapshot.background_jobs = jobs
                .into_iter()
                .filter(|job| !job.status.is_terminal())
                .map(|job| {
                    let group_progress = job.child_count.map(|total| {
                        let terminal = job.children_terminal.unwrap_or(0);
                        format!("{}/{}", terminal, total)
                    });
                    JobLedgerItem {
                        job_id: job.job_id,
                        kind: job.kind.as_str().to_string(),
                        status: job.status.as_str().to_string(),
                        label: (!job.label.is_empty()).then_some(job.label),
                        tool: Some(job.tool),
                        group_progress,
                    }
                })
                .collect();
        }
        Err(e) => snapshot
            .warnings
            .push(format!("background_jobs_snapshot_failed: {}", e)),
    }

    if let Some(db) = crate::globals::get_session_db() {
        match db.list_subagent_runs(session_id) {
            Ok(runs) => {
                snapshot.subagents = runs
                    .into_iter()
                    .filter(|run| {
                        matches!(
                            run.status,
                            SubagentStatus::Queued
                                | SubagentStatus::Spawning
                                | SubagentStatus::Running
                        )
                    })
                    .map(|run| SubagentLedgerItem {
                        run_id: run.run_id,
                        status: run.status.as_str().to_string(),
                        child_agent_id: run.child_agent_id,
                        child_session_id: run.child_session_id,
                        task_preview: crate::truncate_utf8(&run.task, 160).to_string(),
                    })
                    .collect();
            }
            Err(e) => snapshot
                .warnings
                .push(format!("subagent_snapshot_failed: {}", e)),
        }
    } else {
        snapshot
            .warnings
            .push("session_db_unavailable_for_subagent_snapshot".to_string());
    }

    snapshot
}

/// Build the runtime ledger for emergency (Tier 4) compaction, honoring
/// incognito. An incognito session gets `None` so job / subagent ids are never
/// built or injected into the history that Tier 4 both sends to the model and
/// persists via `save_agent_context` — incognito parity with the Tier-3 path in
/// `agent/context.rs`. Callers resolve `is_incognito` via
/// `crate::session::is_session_incognito` (fail-closed) and pass it in, keeping
/// the gate unit-testable without the process-global session DB.
pub(crate) fn emergency_runtime_ledger(
    session_id: &str,
    is_incognito: bool,
) -> Option<RuntimeLedgerSnapshot> {
    if is_incognito {
        None
    } else {
        Some(build_runtime_ledger_snapshot(session_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emergency_ledger_skipped_when_incognito() {
        // Regression guard: the Tier-4 emergency path must NOT build/inject a
        // runtime ledger for an incognito session (job/subagent ids would leak
        // into persisted history). is_incognito=true short-circuits before any
        // global query, so this is deterministic with no globals set.
        assert!(emergency_runtime_ledger("incognito-session", true).is_none());
    }

    #[test]
    fn emergency_ledger_built_when_not_incognito() {
        // Non-incognito → a snapshot is produced (empty/with warnings when the
        // job/session globals are unset in tests, but always `Some`).
        assert!(emergency_runtime_ledger("normal-session", false).is_some());
    }
}
