//! The workflow channel as a host runs it (ADR-0022, ADR-0055): its store
//! in the workspace's state dir, evidence bound to OMP's finished calls in
//! the router log (ADR-0031), and the workspace hashed now as the code
//! state (ADR-0024; row H until evidence binds to buffer versions).

use crate::workflow_store::{self, DiskWorkflowStore};
use crate::{corrections, verify_store, workspace_files};
use cedian_omp::FinishedToolCall;
use cedian_workflow::{BoundCall, CurrentState, WorkflowChannel, WorkflowStatus};
use std::path::Path;
use std::sync::Arc;

/// The channel for `task` over the workspace at `workdir`, with the
/// user's floor and the project's verification profiles. `resolve` binds
/// reported evidence to a finished tool call.
pub fn channel(
    task: &str,
    workdir: &Path,
    state_dir: &Path,
    settings: &crate::Settings,
    resolve: impl Fn(&str, &str) -> Option<BoundCall> + Send + Sync + 'static,
) -> Arc<WorkflowChannel> {
    let root = workdir.to_path_buf();
    WorkflowChannel::with_policy(
        task,
        Box::new(DiskWorkflowStore(state_dir.to_path_buf())),
        resolve,
        move || current_state(&root),
        settings.floor.clone(),
        Box::new(verify_store::DiskProfileStore {
            state_dir: state_dir.to_path_buf(),
            workdir: workdir.to_path_buf(),
        }),
    )
}

/// The most recent call of `tool` (args containing `needle`) that finished
/// without error and is not itself a channel report, with the first later
/// call that may have changed files.
pub fn bound_call(calls: &[FinishedToolCall], tool: &str, needle: &str) -> Option<BoundCall> {
    let at = calls.iter().rposition(|call| {
        !call.is_error
            && call.tool_name == tool
            && call.args_preview.contains(needle)
            && !cedian_workflow::is_channel_call(&call.tool_name, &call.args_preview)
    })?;
    // ADR-0024: a later call that may have changed files means the
    // workspace hashed now is not what this call saw.
    let mutated_after = calls[at + 1..]
        .iter()
        .find(|c| cedian_workflow::may_mutate(&c.tool_name, &c.args_preview))
        .map(|c| format!("{} {}", c.tool_name, c.args_preview));
    let call = calls[at].clone();
    Some(BoundCall {
        tool_call_id: call.tool_call_id,
        tool_name: call.tool_name,
        args_preview: call.args_preview,
        mutated_after,
    })
}

/// The workspace hashed now. Files over the buffer cap are hashed by size
/// and mtime, not read.
pub fn current_state(workdir: &Path) -> CurrentState {
    let files: Vec<(String, Vec<u8>)> = workspace_files::scan_code_state_files(workdir)
        .into_iter()
        .filter_map(|path| {
            let rel = path
                .strip_prefix(workdir)
                .ok()?
                .to_string_lossy()
                .into_owned();
            let meta = std::fs::metadata(&path).ok()?;
            let bytes = if meta.len() > workspace_files::MAX_FILE_BYTES {
                let mtime = meta
                    .modified()
                    .ok()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?
                    .as_nanos();
                format!("{}:{mtime}", meta.len()).into_bytes()
            } else {
                std::fs::read(&path).ok()?
            };
            Some((rel, bytes))
        })
        .collect();
    CurrentState::from_files(
        files
            .iter()
            .map(|(rel, bytes)| (rel.clone(), bytes.as_slice())),
    )
}

/// Turn boundary (ADR-0036): a refused `cedian_complete` in the turn that
/// just ended blocks the workflow (or leaves it failed when the agent
/// failed a phase) and is a `completion_refused` row for `task`'s `turn`.
/// Returns the status and the missing gates; `None` when there is nothing
/// to say (no workflow, or no refused claim).
pub fn end_turn(
    state_dir: &Path,
    task: &str,
    turn: Option<u32>,
) -> Result<Option<(WorkflowStatus, Vec<String>)>, String> {
    if !workflow_store::exists(state_dir) {
        return Ok(None);
    }
    let mut state = workflow_store::load(state_dir)?;
    let Some((status, missing)) = state.end_turn() else {
        if state.last_completion.is_some() {
            workflow_store::save(state_dir, &state)?;
        }
        return Ok(None);
    };
    corrections::record(
        state_dir,
        task,
        corrections::CorrectionKind::CompletionRefused,
        corrections::Event {
            turn,
            excerpt: Some(missing.join("\n")),
            ..corrections::Event::default()
        },
    )?;
    workflow_store::save(state_dir, &state)?;
    Ok(Some((status, missing)))
}
