// ── Runtime Ledger Rendering ────────────────────────────────────
//
// Pure data + markdown rendering for state that can be lost when old tool
// history is summarized away. Live state is gathered by callers outside this
// module; context_compact only renders the snapshot it is handed.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::recovery::FileTouch;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeLedgerSnapshot {
    pub background_jobs: Vec<JobLedgerItem>,
    pub subagents: Vec<SubagentLedgerItem>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobLedgerItem {
    pub job_id: String,
    pub kind: String,
    pub status: String,
    pub label: Option<String>,
    pub tool: Option<String>,
    pub group_progress: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentLedgerItem {
    pub run_id: String,
    pub status: String,
    pub child_agent_id: String,
    pub child_session_id: String,
    pub task_preview: String,
}

fn push_limited_line(out: &mut String, line: &str, budget: usize) -> bool {
    if out.len() + line.len() + 1 > budget {
        return false;
    }
    out.push_str(line);
    out.push('\n');
    true
}

pub fn render_runtime_ledger(
    snapshot: &RuntimeLedgerSnapshot,
    file_touches: &[FileTouch],
    budget_chars: usize,
) -> Option<String> {
    if budget_chars == 0 {
        return None;
    }
    let has_jobs = !snapshot.background_jobs.is_empty();
    let has_subagents = !snapshot.subagents.is_empty();
    let has_files = file_touches.iter().any(|touch| !touch.inlined_by_recovery);
    let has_warnings = !snapshot.warnings.is_empty();
    if !(has_jobs || has_subagents || has_files || has_warnings) {
        return None;
    }

    let mut out = String::new();
    if !push_limited_line(
        &mut out,
        "[Deterministic runtime ledger: as of compaction; current job/subagent status is authoritative via job_status]",
        budget_chars,
    ) {
        return None;
    }
    let mut body_lines = 0usize;

    if has_jobs {
        push_limited_line(&mut out, "\n## Background Jobs", budget_chars);
        for job in &snapshot.background_jobs {
            let mut line = format!("- `{}` {} {}", job.job_id, job.kind, job.status);
            if let Some(tool) = &job.tool {
                line.push_str(&format!(" tool=`{}`", tool));
            }
            if let Some(label) = &job.label {
                if !label.is_empty() {
                    line.push_str(&format!(" label=\"{}\"", label));
                }
            }
            if let Some(progress) = &job.group_progress {
                line.push_str(&format!(" progress={}", progress));
            }
            if !push_limited_line(&mut out, &line, budget_chars) {
                if push_limited_line(
                    &mut out,
                    "- [truncated: more background jobs omitted]",
                    budget_chars,
                ) {
                    body_lines += 1;
                }
                break;
            }
            body_lines += 1;
        }
    }

    if has_subagents {
        push_limited_line(&mut out, "\n## Subagents", budget_chars);
        for run in &snapshot.subagents {
            let line = format!(
                "- `{}` {} agent=`{}` child_session=`{}` task=\"{}\"",
                run.run_id, run.status, run.child_agent_id, run.child_session_id, run.task_preview
            );
            if !push_limited_line(&mut out, &line, budget_chars) {
                if push_limited_line(
                    &mut out,
                    "- [truncated: more subagents omitted]",
                    budget_chars,
                ) {
                    body_lines += 1;
                }
                break;
            }
            body_lines += 1;
        }
    }

    if has_files {
        push_limited_line(&mut out, "\n## Files Touched But Not Inlined", budget_chars);
        for touch in file_touches
            .iter()
            .filter(|touch| !touch.inlined_by_recovery)
        {
            let line = format!(
                "- `{}` last_op={:?} last_seen_index={}",
                touch.path, touch.last_op, touch.last_seen_index
            );
            if !push_limited_line(&mut out, &line, budget_chars) {
                if push_limited_line(
                    &mut out,
                    "- [truncated: more file touches omitted]",
                    budget_chars,
                ) {
                    body_lines += 1;
                }
                break;
            }
            body_lines += 1;
        }
    }

    if has_warnings {
        push_limited_line(&mut out, "\n## Warnings", budget_chars);
        for warning in &snapshot.warnings {
            if !push_limited_line(&mut out, &format!("- {}", warning), budget_chars) {
                break;
            }
            body_lines += 1;
        }
    }

    (body_lines > 0).then_some(out)
}

pub fn build_runtime_ledger_message(
    snapshot: &RuntimeLedgerSnapshot,
    file_touches: &[FileTouch],
    budget_chars: usize,
) -> Option<Value> {
    render_runtime_ledger(snapshot, file_touches, budget_chars).map(|content| {
        serde_json::json!({
            "role": "user",
            "content": content,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_compact::FileOp;

    #[test]
    fn render_skips_file_touches_already_inlined_by_recovery() {
        let snapshot = RuntimeLedgerSnapshot {
            background_jobs: vec![JobLedgerItem {
                job_id: "job-1".to_string(),
                kind: "tool".to_string(),
                status: "running".to_string(),
                label: Some("build".to_string()),
                tool: Some("exec".to_string()),
                group_progress: None,
            }],
            subagents: Vec::new(),
            warnings: Vec::new(),
        };
        let touches = vec![
            FileTouch {
                path: "inlined.rs".to_string(),
                last_op: FileOp::Write,
                last_seen_index: 1,
                inlined_by_recovery: true,
            },
            FileTouch {
                path: "not-inlined.rs".to_string(),
                last_op: FileOp::Edit,
                last_seen_index: 2,
                inlined_by_recovery: false,
            },
        ];

        let rendered = render_runtime_ledger(&snapshot, &touches, 4_000).unwrap();

        assert!(rendered.contains("job-1"));
        assert!(rendered.contains("not-inlined.rs"));
        assert!(!rendered
            .lines()
            .any(|line| line.contains("`inlined.rs` last_op")));
    }

    #[test]
    fn render_honors_zero_budget() {
        let snapshot = RuntimeLedgerSnapshot {
            warnings: vec!["warning".to_string()],
            ..Default::default()
        };

        assert!(render_runtime_ledger(&snapshot, &[], 0).is_none());
    }

    #[test]
    fn render_returns_none_when_budget_cannot_hold_any_state() {
        let snapshot = RuntimeLedgerSnapshot {
            background_jobs: vec![JobLedgerItem {
                job_id: "job-1".to_string(),
                kind: "tool".to_string(),
                status: "running".to_string(),
                label: None,
                tool: Some("exec".to_string()),
                group_progress: None,
            }],
            ..Default::default()
        };

        assert!(render_runtime_ledger(&snapshot, &[], 8).is_none());
        assert!(build_runtime_ledger_message(&snapshot, &[], 8).is_none());
    }
}
