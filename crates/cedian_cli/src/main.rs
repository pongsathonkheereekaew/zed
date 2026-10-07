//! `cedian` CLI harness (throwaway): the full agent loop without GPUI.
//!
//! Wires the headless crates end-to-end in one process:
//! `OmpRuntime` + `HostTools` + `Panel` + `ReviewTracker`. Commands mirror the
//! future palette entries (§2.5) so the wiring transfers to the app shell.
//!
//! Usage:
//! ```text
//! cedian prompt "fix the typo"        # one turn: prompt → stream → cards → review pending
//! cedian review                       # show pending hunks (baseline → current)
//! cedian review reset                 # drop the review task (new baseline next prompt)
//! cedian accept <path> <hunk>         # accept one hunk (mark)
//! cedian reject <path> <hunk>         # reject one hunk (inverse patch)
//! cedian accept-all                   # bulk accept (skips unattributed)
//! cedian state                        # session snapshot (model, streaming, queue)
//! ```
//!
//! State lives in-process per invocation EXCEPT the OMP session (adopted via
//! `--session-dir` + `open_session`), workspace files, and the review task in
//! `review.json` in the state dir (baseline + AgentEdit records + resolutions, see
//! `session.rs`). `cedian review reset` starts a new review task.
#![allow(
    clippy::disallowed_methods,
    reason = "headless, synchronous process control (OMP, LSP, DAP, Chrome, git, sandbox-exec): Zed's async spawn helpers do not apply"
)]

mod audit;
mod browser_store;
mod corrections;
mod revert_turn;
mod review_agent;
mod review_findings;
mod session;
mod shell;
mod shell_lock;
mod state;
mod timing;
mod verify_store;
mod workflow_store;
mod workspace_files;

use cedian_agent_ui::Panel;
use cedian_omp::{ApprovalMode, OmpBinary, OmpRuntime, RuntimeConfig, SpawnPolicy, ToolPolicy};
use cedian_review::{AgentEdit, ReviewTracker};
use cedian_workspace::{HostTools, WorkspaceHost};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn main() {
    let result = run(std::env::args().skip(1).collect());
    if let Err(e) = result {
        eprintln!("cedian: {e}");
        std::process::exit(1);
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    dispatch(args, false)
}

/// Read-only verbs: allowed while a `cedian shell` holds the workspace.
/// Everything else mutates a store or a live resource (ADR-0021 §4).
fn is_read_only(args: &[String]) -> bool {
    let sub = args.get(1).map(String::as_str);
    match args.first().map(String::as_str).unwrap_or("help") {
        "review" => sub.is_none(),
        "state" | "palette" | "symbols" | "diagnostics" | "help" | "shell" | "turns" => true,
        "workflow" => sub == Some("status"),
        "worker" => matches!(sub, Some("list" | "preview")),
        "browser" => sub == Some("status"),
        _ => false,
    }
}

/// Run one command. `in_shell` = issued inside `cedian shell`, which holds
/// the workspace lock itself, so the one-shot lock gate does not apply.
pub(crate) fn dispatch(args: Vec<String>, in_shell: bool) -> Result<(), String> {
    let session_dir = std::env::var("CEDIAN_SESSION_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("cedian-cli-session"));
    // Canonicalize: `/tmp` → `/private/tmp` on macOS (else rootUri,
    // buffer keys, and didOpen URIs disagree and diagnostics never match).
    let workdir: PathBuf = std::env::var("CEDIAN_WORKDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let workdir = workdir.canonicalize().unwrap_or(workdir);

    let cmd = args.first().map(|s| s.as_str()).unwrap_or("help");
    if !in_shell && !is_read_only(&args) {
        shell_lock::refuse_if_shell_live(&workdir, &args.join(" "))?;
    }
    // One settings document per command (the user's `cedian.toml`, ADR-0018).
    // The dangerous tier Deny refuses prompt (the shell rule).
    let settings = cedian_shell::resolve_settings(&workdir).map_err(|e| e.to_string())?;
    match cmd {
        "prompt" => {
            let message = args.get(1).ok_or("usage: cedian prompt <message>")?;
            if settings.permissions.dangerous == cedian_shell::Verdict::Deny {
                return Err("refused: settings [permissions] dangerous = deny".to_string());
            }
            cmd_prompt(&session_dir, &workdir, &settings, message)
        }
        "review" => match args.get(1).map(|s| s.as_str()) {
            None => cmd_review(&workdir),
            Some("reset") => {
                session::reset(&workdir);
                println!("review task reset (next prompt takes a fresh baseline)");
                Ok(())
            }
            Some("dismiss") => {
                let usage = "usage: cedian review dismiss <finding id> <reason>";
                let id = args.get(2).ok_or(usage)?;
                let reason = args[3.min(args.len())..].join(" ");
                println!("{}", review_findings::dismiss(&workdir, id, &reason)?);
                Ok(())
            }
            Some("--agent") => {
                let focus = args[2.min(args.len())..].join(" ");
                let channel = workflow_channel(&workdir, &settings, |_, _| None);
                let requester = review_agent::Requester {
                    channel: Some(&channel),
                    ..Default::default()
                };
                let reply =
                    review_agent::run_review(&workdir, &session_dir, &settings, &focus, requester)?;
                println!("{reply}");
                Ok(())
            }
            Some(other) => Err(format!(
                "usage: cedian review [reset | --agent [focus] | dismiss <id> <reason>] (got {other:?})"
            )),
        },
        "accept" => {
            let path = args.get(1).ok_or("usage: cedian accept <path> <hunk>")?;
            let hunk: usize = args
                .get(2)
                .ok_or("usage: cedian accept <path> <hunk>")?
                .parse()
                .map_err(|_| "bad hunk index")?;
            cmd_accept(&workdir, Path::new(path), hunk)
        }
        "reject" => {
            let path = args.get(1).ok_or("usage: cedian reject <path> <hunk>")?;
            let hunk: usize = args
                .get(2)
                .ok_or("usage: cedian reject <path> <hunk>")?
                .parse()
                .map_err(|_| "bad hunk index")?;
            cmd_reject(&workdir, Path::new(path), hunk)
        }
        "accept-all" => cmd_accept_all(&workdir),
        "turns" => revert_turn::cmd_turns(&workdir),
        "revert-turn" => {
            let which = args.get(1).ok_or("usage: cedian revert-turn <n|last>")?;
            revert_turn::cmd_revert_turn(&workdir, which)
        }
        "state" => cmd_state(&session_dir, &workdir, &settings),
        "shell" if in_shell => Err("already inside cedian shell".to_string()),
        "shell" => shell::run(&session_dir, &workdir, &settings),
        "palette" => {
            let query = args.get(1).map(|s| s.as_str()).unwrap_or("");
            for action in cedian_shell::Palette::filter(query) {
                println!("{} — {} ({})", action.id, action.title, action.hint);
            }
            Ok(())
        }
        "symbols" => {
            let query = args.get(1).ok_or("usage: cedian symbols <query>")?;
            cmd_symbols(&workdir, query)
        }
        "diagnostics" => cmd_diagnostics(&workdir),
        "browser" => cmd_browser(&workdir, &args[1..]),
        "workflow" => cmd_workflow(&workdir, &settings, &args[1..]),
        "worker" => cmd_worker(&workdir, &args[1..]),
        _ => {
            eprintln!(
                "usage: cedian <prompt|shell|review|accept|reject|accept-all|turns|revert-turn|state|\
                palette|symbols|diagnostics|browser|workflow|worker> …"
            );
            eprintln!("env: CEDIAN_SESSION_DIR, CEDIAN_WORKDIR, CEDIAN_OMP_BINARY");
            Ok(())
        }
    }
}

/// Map `[permissions]` onto the OMP spawn policy (ADR-0020). Under the
/// default policy it only tightens: `dangerous = allow` still leaves the exec
/// floor at `prompt` (strict-wins, ADR-0012), and yolo is unrepresentable.
/// Under `policy = "omp"` (ADR-0035) OMP's own config decides approvals;
/// cedian's denies still apply.
fn spawn_policy(
    settings: &cedian_shell::Settings,
    policy_source: cedian_shell::Policy,
) -> SpawnPolicy {
    use cedian_shell::Verdict;
    let mut policy = SpawnPolicy::default();
    if policy_source == cedian_shell::Policy::Omp {
        policy.approvals = cedian_omp::Approvals::Omp;
    }
    for tool in host_tool_names(settings) {
        policy.host_tools.insert(tool.to_string());
    }
    if settings.permissions.project_write != Verdict::Allow
        && policy.approvals != cedian_omp::Approvals::Omp
    {
        policy.approvals = cedian_omp::Approvals::Cedian(ApprovalMode::AlwaysAsk);
    }
    let mut deny = |tools: &[&str]| {
        for tool in tools {
            policy
                .tool_policies
                .insert((*tool).to_string(), ToolPolicy::Deny);
        }
    };
    if settings.permissions.project_write == Verdict::Deny {
        deny(&["edit", "write", "ast_edit"]);
    }
    if settings.permissions.dangerous == Verdict::Deny {
        deny(cedian_omp::spawn_profile::EXEC_TOOLS);
    }
    policy
}

/// The complete cedian host-tool set for these settings (ADR-0004: one
/// `set_host_tools` call replaces the whole set, so register all of it).
/// `cedian_worktree_request` writes `.worktrees/` + a branch: absent when
/// project writes are denied.
fn host_tool_names(settings: &cedian_shell::Settings) -> Vec<&'static str> {
    let mut names = vec![
        cedian_workspace::APPLY_EDIT_TOOL,
        cedian_workflow::WORKFLOW_UPDATE_TOOL,
        cedian_workflow::COMPLETE_TOOL,
        review_agent::REVIEW_REQUEST_TOOL,
        corrections::CORRECTION_CLASS_TOOL,
    ];
    if settings.permissions.project_write != cedian_shell::Verdict::Deny {
        names.push(cedian_worker::WORKTREE_REQUEST_TOOL);
    }
    names
}

/// The workflow channel over `workdir`'s store, with the user's floor and
/// the project's verification profiles. `resolve` binds reported evidence
/// to a finished tool call; the CLI's own paths pass one that binds none.
fn workflow_channel(
    workdir: &Path,
    settings: &cedian_shell::Settings,
    resolve: impl Fn(&str, &str) -> Option<cedian_workflow::BoundCall> + Send + Sync + 'static,
) -> std::sync::Arc<cedian_workflow::WorkflowChannel> {
    let root = workdir.to_path_buf();
    cedian_workflow::WorkflowChannel::with_policy(
        "cli",
        Box::new(DiskWorkflowStore(workdir.to_path_buf())),
        resolve,
        move || current_state(&root),
        settings.floor.clone(),
        Box::new(verify_store::DiskProfileStore(workdir.to_path_buf())),
    )
}

/// Turn boundary (ADR-0036): a refused `cedian_complete` in this turn
/// blocks the workflow (or leaves it failed when the agent failed a phase);
/// say so with the missing gates.
fn end_workflow_turn(workdir: &Path) -> Result<(), String> {
    if !workflow_store::exists(workdir) {
        return Ok(());
    }
    let mut state = workflow_store::load(workdir)?;
    let Some((status, missing)) = state.end_turn() else {
        if state.last_completion.is_some() {
            workflow_store::save(workdir, &state)?;
        }
        return Ok(());
    };
    let next_turn = session::load(workdir)?
        .map(|s| s.turns.len() as u32 + 1)
        .unwrap_or(1);
    corrections::record(
        workdir,
        corrections::CorrectionKind::CompletionRefused,
        corrections::Event {
            turn: Some(next_turn),
            excerpt: Some(missing.join("\n")),
            ..corrections::Event::default()
        },
    )?;
    workflow_store::save(workdir, &state)?;
    let (word, next) = if status == cedian_workflow::WorkflowStatus::Failed {
        ("FAILED", "start a new workflow to retry")
    } else {
        ("BLOCKED", "`cedian workflow resume` to continue")
    };
    println!("[!] workflow {word}: the agent claimed done with required gates unmet");
    for m in &missing {
        println!("    - {m}");
    }
    println!("    (`cedian workflow status` for the claims; {next})");
    Ok(())
}

/// The workspace hashed now (ADR-0024; headless code state, row H). Files
/// over the buffer cap are hashed by size and mtime, not read.
fn current_state(workdir: &Path) -> cedian_workflow::CurrentState {
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
    cedian_workflow::CurrentState::from_files(
        files
            .iter()
            .map(|(rel, bytes)| (rel.clone(), bytes.as_slice())),
    )
}

/// `workflow.json` in the state dir as the channel's store.
struct DiskWorkflowStore(PathBuf);

impl cedian_workflow::WorkflowStore for DiskWorkflowStore {
    fn load(&self) -> Result<Option<cedian_workflow::WorkflowState>, String> {
        if !workflow_store::exists(&self.0) {
            return Ok(None);
        }
        workflow_store::load(&self.0).map(Some)
    }
    fn save(&self, state: &cedian_workflow::WorkflowState) -> Result<(), String> {
        workflow_store::save(&self.0, state)
    }
}

/// Every host tool, with the P5 evidence resolver over this runtime's router
/// log: evidence binds to the most recent call of the named tool (args
/// containing `needle`) that finished without error and is not itself a
/// channel report (ADR-0022, ADR-0031).
fn host_tools(
    rt: &OmpRuntime,
    session_dir: &Path,
    workdir: &Path,
    settings: &cedian_shell::Settings,
    host: &std::sync::Arc<HostTools>,
) -> Result<Vec<omp_rpc::HostTool>, String> {
    let router = rt.router();
    let resolve = move |tool: &str, needle: &str| {
        let calls = router.finished_tool_calls();
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
        Some(cedian_workflow::BoundCall {
            tool_call_id: call.tool_call_id,
            tool_name: call.tool_name,
            args_preview: call.args_preview,
            mutated_after,
        })
    };
    let channel = workflow_channel(workdir, settings, resolve);
    let root = workdir.to_path_buf();
    let live = std::sync::Arc::clone(host);
    channel.set_blockers(move || {
        // Bring this turn's edits in first: a hunk fixed this turn is fixed.
        match flush_turn_for_review(&root, &live) {
            Ok(()) => review_findings::open_blockers(&root),
            Err(e) => vec![format!("review: cannot bring this turn's edits in ({e})")],
        }
    });
    let names = host_tool_names(settings);
    let mut tools = vec![host.apply_edit_tool()];
    tools.extend(channel.host_tools());
    tools.push(review_agent::review_request_tool(
        workdir.to_path_buf(),
        session_dir.to_path_buf(),
        settings.clone(),
        host.clone(),
        rt.router(),
        std::sync::Arc::clone(&channel),
    ));
    let class_root = workdir.to_path_buf();
    tools.push(corrections::correction_class_tool(
        workdir.to_path_buf(),
        move || current_state(&class_root),
    ));
    if names.contains(&cedian_worker::WORKTREE_REQUEST_TOOL) {
        tools.push(cedian_worker::worktree_request_tool(
            workdir.to_path_buf(),
            state::dir(workdir)?,
        ));
    }
    debug_assert_eq!(tools.len(), names.len());
    Ok(tools)
}

/// The opt-in badge (ADR-0035 decision 4): OMP's effective approval mode
/// and `computer`, and whether the workspace's own `.omp/config.yml` set them
/// (OMP's answer in the workspace differs from its answer in an empty dir).
fn omp_policy_badge(workdir: &Path) -> String {
    let binary = omp_binary_path();
    let empty = std::env::temp_dir().join(format!("cedian-omp-global-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&empty);
    let get = |cwd: &Path, key: &str| {
        let binary = binary.as_ref().ok()?;
        cedian_omp::omp_config_get(binary, cwd, key, Duration::from_secs(10)).ok()
    };
    let describe = |key: &str, show: &dyn Fn(&serde_json::Value) -> String| {
        let here = get(workdir, key);
        let from = if here.is_some() && here != get(&empty, key) {
            " from the project's .omp/config.yml"
        } else {
            ""
        };
        match here {
            Some(v) => format!("{}{from}", show(&v)),
            None => "unknown, assume on".to_string(),
        }
    };
    let mode = describe("tools.approvalMode", &|v| {
        v.as_str().unwrap_or("unknown").to_string()
    });
    let computer = describe("computer.enabled", &|v| {
        if v.as_bool() == Some(true) {
            "on"
        } else {
            "off"
        }
        .to_string()
    });
    let _ = std::fs::remove_dir(&empty);
    format!(
        "◆ OMP policy — approvals and computer from your OMP config (approvalMode: {mode}; computer: {computer})"
    )
}

/// The `omp` binary a spawn will run (`CEDIAN_OMP_BINARY`, else `PATH`).
fn omp_binary_path() -> Result<PathBuf, String> {
    match std::env::var("CEDIAN_OMP_BINARY") {
        Ok(path) => Ok(PathBuf::from(path)),
        Err(_) => cedian_omp::resolve_on_path("omp", std::env::var("PATH").ok().as_deref())
            .map_err(|e| e.to_string()),
    }
}

/// Tools OMP's merged config sets to `allow` in `workdir` (ADR-0041
/// decision 2). Fails closed: without the record the default profile cannot
/// pin them, so the spawn is refused.
fn omp_config_allows(workdir: &Path) -> Result<std::collections::BTreeSet<String>, String> {
    let binary = omp_binary_path()?;
    let record =
        cedian_omp::omp_config_get(&binary, workdir, "tools.approval", Duration::from_secs(10))
            .map_err(|e| format!("cannot read OMP's tools.approval, so cannot pin it: {e}"))?;
    let record = record
        .as_object()
        .ok_or("OMP's tools.approval is not a record")?;
    Ok(record
        .iter()
        .filter(|(_, v)| v.as_str() == Some("allow"))
        .map(|(k, _)| k.clone())
        .collect())
}

/// Spawn the runtime with workspace host tools + cedian:// wired.
fn spawn(
    session_dir: &Path,
    workdir: &Path,
    settings: &cedian_shell::Settings,
    host: &std::sync::Arc<HostTools>,
) -> Result<OmpRuntime, String> {
    // `CEDIAN_OMP_BINARY` (absolute) points the harness at another binary —
    // the hermetic replay lane uses it for fake-omp (§86, P2).
    let binary = match std::env::var("CEDIAN_OMP_BINARY") {
        Ok(path) => OmpBinary::Bundled(PathBuf::from(path)),
        Err(_) => OmpBinary::Path("omp".to_string()),
    };
    // Every spawn here has a person at the terminal; reviewers and
    // automations will pass `RunKind::Unattended` (ADR-0035 decision 5).
    let choice = settings.policy_for(workdir, cedian_shell::RunKind::Interactive);
    for note in &choice.notes {
        println!("note: {note}");
    }
    if choice.policy == cedian_shell::Policy::Omp {
        println!("{}", omp_policy_badge(workdir));
        let p = &settings.permissions;
        if [p.safe, p.project_write, p.dangerous].contains(&cedian_shell::Verdict::Ask) {
            println!(
                "note: [permissions] ask tiers do not apply under policy = \"omp\" (OMP decides); deny tiers still do"
            );
        }
    }
    let mut policy = spawn_policy(settings, choice.policy);
    if choice.policy == cedian_shell::Policy::Cedian {
        policy.config_allows = omp_config_allows(workdir)?;
    }
    let rt = OmpRuntime::spawn(RuntimeConfig {
        binary,
        session_dir: session_dir.to_path_buf(),
        cwd: workdir.to_path_buf(),
        ask_dialog: true,
        prompt_timeout: Duration::from_secs(600),
        policy,
    })
    .map_err(|e| e.to_string())?;
    // Nothing in the CLI can answer an OMP dialog: refuse at once (P5 gap).
    rt.deny_ui_requests();
    rt.set_host_tools(host_tools(&rt, session_dir, workdir, settings, host)?)
        .map_err(|e| e.to_string())?;
    rt.set_host_uris(vec![host.cedian_uri_scheme()])
        .map_err(|e| e.to_string())?;
    Ok(rt)
}

/// Load every text file under workdir into the host buffers (headless scan).
/// Re-reads buffers that disk moved past: in `cedian shell` the host lives
/// across turns, and OMP's own writes land on disk (row G). A buffer with
/// unsaved host edits is kept and reported.
fn load_workspace(host: &HostTools, workdir: &Path) -> Vec<PathBuf> {
    workspace_files::scan_text_files(workdir)
        .into_iter()
        .filter_map(|path| {
            let key = workspace_files::buffer_key(workdir, &path)?;
            let text = std::fs::read_to_string(&path).ok()?;
            if !host.reload(&key, &text) {
                eprintln!(
                    "{}: unsaved buffer edits kept over a newer disk text",
                    key.display()
                );
            }
            Some(key)
        })
        .collect()
}

/// One ambient line while a workflow is open (ADR-0022: the model must be
/// told when to call the channel; compliance is checked, never assumed).
fn workflow_ambient(workdir: &Path) -> Option<String> {
    use cedian_workflow::WorkflowStatus;
    if !workflow_store::exists(workdir) {
        return None;
    }
    let state = workflow_store::load(workdir).ok()?;
    if !matches!(
        state.status,
        WorkflowStatus::Running | WorkflowStatus::Blocked
    ) {
        return None;
    }
    Some(format!(
        "cedian workflow active: {:?} ({:?}, {:?}, phase {}). Report evidence with {} \
         (from_tool = the tool whose call produced it) and call {} before saying it is done.",
        state.task.title,
        state.task.kind,
        state.status,
        state.current_phase.as_deref().unwrap_or("-"),
        cedian_workflow::WORKFLOW_UPDATE_TOOL,
        cedian_workflow::COMPLETE_TOOL,
    ))
}

/// Tools whose successful completion may have changed files on disk.
const EDIT_TOOLS: &[&str] = &["edit", "write", "ast_edit", "cedian_apply_edit"];

fn cmd_prompt(
    session_dir: &Path,
    workdir: &Path,
    settings: &cedian_shell::Settings,
    message: &str,
) -> Result<(), String> {
    let host = HostTools::shared(workdir);
    let started = Instant::now();
    let mut rt = spawn(session_dir, workdir, settings, &host)?;
    rt.open_session("cli").map_err(|e| e.to_string())?;
    timing::record(serde_json::json!({"event": "spawn", "ms": timing::ms(started.elapsed())}));
    let result = run_turn(
        &mut rt,
        &host,
        workdir,
        message,
        false,
        session::TurnKind::Prompt,
        message,
    );
    let shutdown = rt.shutdown();
    result?;
    shutdown.map_err(|e| e.to_string())
}

/// One OMP turn on an already-running runtime: prompt → cards → write-back →
/// provenance → review store + turn log. Shared by one-shot `prompt` and
/// `cedian shell`. `live` streams assistant text to stdout as it arrives (the
/// shell). `kind` + `label` describe the turn in `cedian turns`.
pub(crate) fn run_turn(
    rt: &mut OmpRuntime,
    host: &std::sync::Arc<HostTools>,
    workdir: &Path,
    message: &str,
    live: bool,
    kind: session::TurnKind,
    label: &str,
) -> Result<(), String> {
    let turn_started = Instant::now();
    let mut store = session::load(workdir)?.unwrap_or_default();
    let keys = load_workspace(host, workdir);

    // Pre-turn texts (disk == buffer right after load). The task baseline for
    // a file is its pre-turn text the FIRST time a turn changes it (§16), so
    // the store holds only files this task touched — user edits to other
    // files never enter review.
    let mut pre: HashMap<PathBuf, String> = HashMap::new();
    for key in &keys {
        if let Some(t) = host.read_buffer(key) {
            pre.insert(key.clone(), t);
        }
    }
    let _turn = TurnPre::set(&pre);

    let mut panel = Panel::new();
    let task_id = panel.new_task("cli", workdir.to_path_buf());
    let router = rt.router();
    let (sub, rx) = router.subscribe();
    let (settled_tx, settled_rx) = std::sync::mpsc::channel::<()>();

    let approvals = rt.approvals();
    let mut audit = audit::AuditLog::open(workdir, approvals)?;

    // Pump router events into the panel on a thread while the turn runs.
    let pump = std::thread::spawn(move || {
        use std::io::Write as _;
        let mut panel = panel;
        let mut audit_error = None;
        for event in rx.iter() {
            if let Err(e) = audit.record(&event) {
                audit_error.get_or_insert(e);
            }
            if let cedian_omp::RouterEvent::MessageDelta {
                kind: cedian_omp::DeltaKind::Text,
                delta,
                ..
            } = &event
            {
                if live {
                    print!("{delta}");
                    let _ = std::io::stdout().flush();
                }
            }
            let done = matches!(event, cedian_omp::RouterEvent::Settled);
            panel.dispatch(&event);
            if done {
                let _ = settled_tx.send(());
                break;
            }
        }
        (panel, audit, audit_error)
    });

    // Ambient context travels with the prompt (§39).
    let ambient =
        cedian_workspace::render_snapshot(&cedian_workspace::capture_ambient(host.as_ref()));
    let ambient = match workflow_ambient(workdir) {
        Some(line) => format!("{ambient}{line}\n"),
        None => ambient,
    };
    let full = if ambient.is_empty() {
        message.to_string()
    } else {
        format!("{ambient}\n{message}")
    };
    let context_ms = timing::ms(turn_started.elapsed());
    let omp_started = Instant::now();
    let turn = rt.prompt(&full, vec![]);
    let omp_ms = timing::ms(omp_started.elapsed());
    // `Settled` follows `prompt_result`; give it a moment, then unsubscribe —
    // dropping our sender always ends the pump, even when the turn failed or
    // no `Settled` arrived.
    if turn.is_ok() {
        let _ = settled_rx.recv_timeout(Duration::from_secs(5));
    }
    router.unsubscribe(sub);
    drop(router);
    let (panel, mut audit, mut audit_error) =
        pump.join().map_err(|_| "pump thread died".to_string())?;
    let refused = rt.take_refused_ui_requests();
    for r in &refused {
        if let Err(e) = audit.refusal(r) {
            audit_error.get_or_insert(e);
        }
    }
    turn.map_err(|e| e.to_string())?;
    if let Some(e) = audit_error {
        return Err(format!(
            "{e} — the turn is not audited, so it fails (ADR-0035)"
        ));
    }

    // Render the turn from the thread the stream built, not from
    // `prompt_result`'s copy of the text (S0 exit).
    let task = panel.get(&task_id).ok_or("task vanished")?;
    let (messages, cards) = cedian_agent_ui::render_thread(task.thread().events());
    if live {
        println!();
    } else {
        for m in messages
            .iter()
            .filter(|m| m.role == cedian_agent_ui::MessageRole::Assistant && !m.text.is_empty())
        {
            println!("{}", m.text);
        }
    }
    for card in &cards {
        // Headless refuses every dialog, so an exec-tier call that completed
        // under the opt-in was approved by OMP's config, not by a person. A
        // call OMP blocks still starts and then ends in error: no label.
        let label = if approvals == cedian_omp::Approvals::Omp
            && cedian_omp::spawn_profile::EXEC_TOOLS.contains(&card.name.as_str())
            && card.status == cedian_agent_ui::ToolCardStatus::Done
        {
            " · approved by OMP"
        } else {
            ""
        };
        println!("[{}] {}{label}", card.status_glyph(), card.display_line());
    }
    for r in &refused {
        println!("[✗] refused (no UI to approve): {}", r.label);
    }
    end_workflow_turn(workdir)?;

    // Write back ONLY buffers cedian itself changed (host-tool edits), and
    // never over a file that changed on disk during the turn: OMP's native
    // `edit`/`write` write the filesystem directly (plan §84 row G), so disk
    // is authoritative for them.
    let mut conflicts = Vec::new();
    let mut synced = 0;
    for key in &keys {
        let (Some(before), Some(buffer)) = (pre.get(key), host.read_buffer(key)) else {
            continue;
        };
        if &buffer == before {
            continue;
        }
        let Some(local) = workspace_files::local_path(workdir, key) else {
            continue;
        };
        let disk = std::fs::read_to_string(&local).unwrap_or_default();
        if disk == buffer {
            continue; // already flushed for a mid-turn review
        }
        if &disk != before {
            conflicts.push(key.display().to_string());
            continue;
        }
        std::fs::write(&local, &buffer).map_err(|e| e.to_string())?;
        synced += 1;
    }
    if synced > 0 {
        eprintln!("(synced {synced} buffer(s) to disk)");
    }
    for c in &conflicts {
        eprintln!(
            "conflict: {c} changed on disk AND in the buffer during the turn — buffer NOT written"
        );
    }

    // Provenance (§17): snapshot every file the turn changed, read from disk
    // (new files included). Pin to a tool call when one edit-class card names
    // the file, or when the turn had exactly one; otherwise no record → the
    // hunks review as UNATTRIBUTED (never misattributed).
    let edit_cards: Vec<&cedian_agent_ui::ToolCard> = cards
        .iter()
        .filter(|c| {
            c.status == cedian_agent_ui::ToolCardStatus::Done
                && EDIT_TOOLS.contains(&c.name.as_str())
        })
        .collect();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut unattributed = 0;
    let mut turn_files = Vec::new();
    for (key, before, after) in changed_since(workdir, &pre) {
        store.baseline_once(&key, &before); // first change in task (new file: empty)
        turn_files.push(session::TurnFile {
            file: key.to_string_lossy().into_owned(),
            before: before.clone(),
            after: after.clone(),
            created: !pre.contains_key(&key),
        });
        let rel = key.to_string_lossy().trim_start_matches('/').to_string();
        let pinned =
            edit_cards
                .iter()
                .find(|c| c.preview.contains(&rel))
                .or(if edit_cards.len() == 1 {
                    edit_cards.first()
                } else {
                    None
                });
        match pinned {
            Some(card) => store.record(AgentEdit {
                tool_call_id: card.call_id.clone(),
                task_id: session::CLI_TASK.to_string(),
                file: key.to_string_lossy().into_owned(),
                before,
                after,
                timestamp_ms: now_ms,
            }),
            None => unattributed += 1,
        }
    }
    if unattributed > 0 {
        eprintln!(
            "({unattributed} changed file(s) not pinned to a tool call → UNATTRIBUTED in review)"
        );
    }
    if let Some(n) = store.record_turn(kind, label, turn_files) {
        println!("(turn {n} recorded — `revert-turn {n}` puts it back)");
    }
    store.models.extend(rt.router().answered_models());
    let saved = session::save(workdir, &store);
    let total_ms = timing::ms(turn_started.elapsed());
    timing::record(serde_json::json!({
        "event": "turn", "context_ms": context_ms, "omp_ms": omp_ms,
        "post_ms": total_ms.saturating_sub(context_ms + omp_ms), "total_ms": total_ms,
    }));
    saved
}

/// Files on disk whose text differs from their pre-turn text (a new file's
/// pre-turn text is empty): `(key, before, after)`.
fn changed_since(workdir: &Path, pre: &HashMap<PathBuf, String>) -> Vec<(PathBuf, String, String)> {
    workspace_files::scan_text_files(workdir)
        .into_iter()
        .filter_map(|path| {
            let key = workspace_files::buffer_key(workdir, &path)?;
            let after = std::fs::read_to_string(&path).ok()?;
            let before = pre.get(&key).cloned().unwrap_or_default();
            (after != before).then_some((key, before, after))
        })
        .collect()
}

/// The running turn's pre-turn texts, so a review asked for mid-turn
/// (`cedian_review_request`) can see this turn's changes. One turn runs at a
/// time in this process; the guard clears it when the turn ends.
static TURN_PRE: std::sync::Mutex<Option<HashMap<PathBuf, String>>> = std::sync::Mutex::new(None);

struct TurnPre;

impl TurnPre {
    fn set(pre: &HashMap<PathBuf, String>) -> Self {
        *TURN_PRE.lock().unwrap_or_else(|e| e.into_inner()) = Some(pre.clone());
        Self
    }
}

impl Drop for TurnPre {
    fn drop(&mut self) {
        *TURN_PRE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Bring this turn's changes into review before a mid-turn review: write
/// back buffers cedian changed (only where disk still holds the pre-turn
/// text, row G) and baseline every file the turn changed. Provenance is
/// still recorded when the turn ends. A no-op between turns.
fn flush_turn_for_review(workdir: &Path, host: &HostTools) -> Result<(), String> {
    let Some(pre) = TURN_PRE.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
        return Ok(());
    };
    for (key, before) in &pre {
        let Some(buffer) = host.read_buffer(key) else {
            continue;
        };
        let Some(local) = workspace_files::local_path(workdir, key) else {
            continue;
        };
        if &buffer != before && std::fs::read_to_string(&local).ok().as_ref() == Some(before) {
            std::fs::write(&local, &buffer).map_err(|e| e.to_string())?;
        }
    }
    let mut store = session::load(workdir)?.unwrap_or_default();
    for (key, before, _) in changed_since(workdir, &pre) {
        store.baseline_once(&key, &before);
    }
    session::save(workdir, &store)
}

/// Load the review task + tracker, rebuilt against current disk state.
/// No task yet → empty tracker (review shows nothing).
fn load_tracker(
    workdir: &Path,
    host: &HostTools,
) -> Result<(session::ReviewStore, ReviewTracker), String> {
    load_workspace(host, workdir);
    let store = session::load(workdir)?.unwrap_or_default();
    let (baseline, texts) = store.tracker_inputs();
    // Files in the baseline that no longer exist on disk review as deletions.
    for key in texts.keys() {
        if host.read_buffer(key).is_none() {
            host.open(key, "");
        }
    }
    let mut tracker = ReviewTracker::new(&store.task_id, baseline, texts);
    tracker.attribute(&store.provenance);
    tracker.restore_statuses(store.statuses.clone());
    tracker.rebuild_now(host as &dyn WorkspaceHost);
    // ADR-0032: a user edit over an agent hunk is a correction (once per text).
    for path in tracker.paths() {
        let Ok(diff) = tracker.diff(&path) else {
            continue;
        };
        for (i, status) in diff.statuses.iter().enumerate() {
            if *status != cedian_review::HunkStatus::Stale {
                continue;
            }
            let key = path.to_string_lossy().into_owned();
            corrections::record(
                workdir,
                corrections::CorrectionKind::UserEditedAgentHunk,
                corrections::Event {
                    turn: store.last_turn_touching(&key),
                    hunk_key: Some(format!("{key}#{i}")),
                    path: Some(key),
                    excerpt: hunk_text(&tracker, &path, i),
                    ..corrections::Event::default()
                },
            )?;
        }
    }
    Ok((store, tracker))
}

/// A hunk's after-lines, the text a correction row hashes.
fn hunk_text(tracker: &ReviewTracker, path: &Path, hunk: usize) -> Option<String> {
    let diff = tracker.diff(path).ok()?;
    let h = diff.hunks.get(hunk)?;
    Some(
        diff.snapshot
            .lines()
            .skip(h.after_start)
            .take(h.after_count)
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Persist the tracker's resolutions back into the review task.
fn save_statuses(
    workdir: &Path,
    mut store: session::ReviewStore,
    tracker: &ReviewTracker,
) -> Result<(), String> {
    store.statuses = tracker.status_records();
    session::save(workdir, &store)
}

fn cmd_review(workdir: &Path) -> Result<(), String> {
    let host = HostTools::new(workdir);
    let (_store, tracker) = load_tracker(workdir, &host)?;
    let mut any = false;
    for path in tracker.paths() {
        let diff = tracker.diff(&path).map_err(|e| e.to_string())?;
        if diff.is_empty() {
            continue;
        }
        any = true;
        println!("{} ({} hunk(s))", diff.path, diff.len());
        for (i, hunk) in diff.hunks.iter().enumerate() {
            println!(
                "  [{}] hunk {i}: -{}+{} → +{}+{} [{}]",
                i,
                hunk.before_start + 1,
                hunk.before_count,
                hunk.after_start + 1,
                hunk.after_count,
                status_glyph(diff.statuses[i])
            );
        }
    }
    if !any {
        println!("no pending changes (baseline == current)");
    }
    let findings = review_findings::states(workdir)?;
    if !findings.is_empty() {
        println!("findings:");
        for (f, state) in findings {
            println!(
                "  {} [{state}] {:?} {} hunk {}: {}",
                f.id, f.finding.severity, f.finding.path, f.hunk, f.finding.message
            );
        }
    }
    Ok(())
}

fn cmd_accept(workdir: &Path, path: &Path, hunk: usize) -> Result<(), String> {
    let host = HostTools::new(workdir);
    let (store, mut tracker) = load_tracker(workdir, &host)?;
    tracker.accept_hunk(path, hunk).map_err(|e| e.to_string())?;
    save_statuses(workdir, store, &tracker)?;
    println!(
        "accepted {} hunk {hunk} (marked; code already in buffer)",
        path.display()
    );
    Ok(())
}

fn cmd_reject(workdir: &Path, path: &Path, hunk: usize) -> Result<(), String> {
    let host = HostTools::new(workdir);
    let (store, mut tracker) = load_tracker(workdir, &host)?;
    let rejected = hunk_text(&tracker, path, hunk);
    let v = tracker
        .reject_hunk(path, hunk, &host as &dyn WorkspaceHost)
        .map_err(|e| e.to_string())?;
    let key = path.to_string_lossy().into_owned();
    corrections::record(
        workdir,
        corrections::CorrectionKind::HunkRejected,
        corrections::Event {
            turn: store.last_turn_touching(&key),
            hunk_key: Some(format!("{key}#{hunk}")),
            tool_call_id: store
                .provenance
                .iter()
                .rev()
                .find(|e| e.file == key)
                .map(|e| e.tool_call_id.clone()),
            path: Some(key),
            excerpt: rejected,
        },
    )?;
    // Sync the restored buffer back to disk.
    if let Some(local) = workspace_files::local_path(workdir, path) {
        if let Some(text) = host.read_buffer(path) {
            std::fs::write(&local, text).map_err(|e| e.to_string())?;
        }
    }
    save_statuses(workdir, store, &tracker)?;
    println!(
        "rejected {} hunk {hunk} (inverse patch → v{})",
        path.display(),
        v.0
    );
    Ok(())
}

fn cmd_accept_all(workdir: &Path) -> Result<(), String> {
    let host = HostTools::new(workdir);
    let (store, mut tracker) = load_tracker(workdir, &host)?;
    let accepted = tracker.accept_all();
    save_statuses(workdir, store, &tracker)?;
    println!("accepted {} hunk(s) (unattributed skipped)", accepted.len());
    for (path, i) in accepted {
        println!("  {} hunk {i}", path.display());
    }
    Ok(())
}

fn cmd_state(
    session_dir: &Path,
    workdir: &Path,
    settings: &cedian_shell::Settings,
) -> Result<(), String> {
    let host = HostTools::shared(workdir);
    let rt = spawn(session_dir, workdir, settings, &host)?;
    let state = rt.get_state().map_err(|e| e.to_string())?;
    let (model, thinking) = cedian_agent::state::model_from_state(&state);
    println!("model: {}/{}", model.provider, model.id);
    println!("thinking: {:?}", thinking);
    println!("streaming: {}", state.is_streaming);
    println!("settled: {}", state.is_settled);
    println!("messages: {}", state.message_count);
    rt.shutdown().map_err(|e| e.to_string())?;
    Ok(())
}

/// Spawn rust-analyzer for the workdir, attach to a host, sync open buffers.
/// Returns (host, bridge) — bridge owns the server child.
fn spawn_lsp(
    workdir: &Path,
) -> Result<(std::sync::Arc<HostTools>, cedian_workspace::LspBridge), String> {
    let host = HostTools::shared(workdir);
    for path in crate::workspace_files::scan_text_files(workdir) {
        if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            if let Ok(text) = std::fs::read_to_string(&path) {
                let key = crate::workspace_files::buffer_key(workdir, &path)
                    .ok_or_else(|| "path escapes workspace".to_string())?;
                host.open(&key, &text);
            }
        }
    }
    let bridge = cedian_workspace::LspBridge::spawn("rust-analyzer", workdir, &host)
        .map_err(|e| e.to_string())?;
    // Sync every open buffer into the server.
    for key in host.open_keys() {
        if let Some(text) = host.read_buffer(&key) {
            let local = workdir.join(key.strip_prefix("/").unwrap_or(&key));
            bridge.sync_buffer(&local, &text);
        }
    }
    Ok((host, bridge))
}

fn cmd_symbols(workdir: &Path, query: &str) -> Result<(), String> {
    let (_host, bridge) = spawn_lsp(workdir)?;
    // Workspace index builds async — retry empty answers until the deadline.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let symbols = bridge.workspace_symbols(query).map_err(|e| e.to_string())?;
        if !symbols.is_empty() || std::time::Instant::now() >= deadline {
            if symbols.is_empty() {
                println!("no symbols for {query:?}");
            }
            for line in cedian_workspace::lsp_bridge::render_workspace_symbols(&symbols).lines() {
                println!("{line}");
            }
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}

fn cmd_diagnostics(workdir: &Path) -> Result<(), String> {
    let (host, _bridge) = spawn_lsp(workdir)?;
    // Wait for the first publishDiagnostics wave. Cold rust-analyzer init
    // (cargo metadata + first check) can take 5+ min on first launch of the
    // day; warm restarts answer in seconds. The bridge caches nothing — every
    // CLI invocation pays cold start once (S9 keeps one warm server).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(900);
    loop {
        let mut any = false;
        for key in host.open_keys() {
            if !host.diagnostics(&key).is_empty() {
                any = true;
                break;
            }
        }
        if any || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
    let mut shown = false;
    for key in host.open_keys() {
        for d in host.diagnostics(&key) {
            shown = true;
            let sev = match d.severity {
                cedian_workspace::DiagnosticSeverity::Error => "error",
                cedian_workspace::DiagnosticSeverity::Warning => "warning",
                cedian_workspace::DiagnosticSeverity::Info => "info",
            };
            println!("{sev} {}:{} {}", d.path.display(), d.line, d.message);
        }
    }
    if !shown {
        println!("no diagnostics (clean)");
    }
    Ok(())
}

/// Workflow commands (S2 headless surface):
/// ```text
/// cedian workflow run <kind> <title> [--risk low|medium|high]  # start (overwrites)
/// cedian workflow status                                       # §51 render + gates
/// cedian workflow evidence <gate> <summary> [--fail|--inconclusive]  # always unattributed
/// cedian workflow advance [--fail]                             # pass/fail current phase
/// cedian workflow resume                                       # unblock (ADR-0036)
/// cedian workflow complete                                     # §55 completion gate
/// ```
fn cmd_workflow(
    workdir: &Path,
    settings: &cedian_shell::Settings,
    args: &[String],
) -> Result<(), String> {
    match args.first().map(|s| s.as_str()) {
        Some("run") => {
            let kind = args
                .get(1)
                .ok_or("usage: cedian workflow run <kind> <title> [--risk R]")?;
            let title = args
                .get(2)
                .ok_or("usage: cedian workflow run <kind> <title> [--risk R]")?;
            let kind = match kind.as_str() {
                "investigation" => cedian_workflow::TaskKind::Investigation,
                "bug_fix" => cedian_workflow::TaskKind::BugFix,
                "feature" => cedian_workflow::TaskKind::Feature,
                "refactor" => cedian_workflow::TaskKind::Refactor,
                "performance" => cedian_workflow::TaskKind::Performance,
                "prototype" => cedian_workflow::TaskKind::Prototype,
                _ => {
                    return Err(format!(
                        "unknown kind {kind:?} (investigation|bug_fix|feature|refactor|performance|prototype)"
                    ));
                }
            };
            let mut profile = cedian_workflow::TaskProfile::new(title, kind);
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--risk" => {
                        let r = args.get(i + 1).ok_or("usage: --risk low|medium|high")?;
                        profile.risk = match r.as_str() {
                            "low" => cedian_workflow::Risk::Low,
                            "medium" => cedian_workflow::Risk::Medium,
                            "high" => cedian_workflow::Risk::High,
                            _ => return Err(format!("unknown risk {r:?}")),
                        };
                        i += 2;
                    }
                    flag => return Err(format!("unknown flag {flag:?}")),
                }
            }
            let state = cedian_workflow::WorkflowState::start_with_floor(profile, &settings.floor)
                .map_err(|e| e.to_string())?;
            workflow_store::save(workdir, &state)?;
            render_workflow(&state, &current_state(workdir));
            Ok(())
        }
        Some("status") => {
            let state = workflow_store::load(workdir)?;
            render_workflow(&state, &current_state(workdir));
            Ok(())
        }
        Some("evidence") => {
            const USAGE: &str =
                "usage: cedian workflow evidence <gate> <summary> [--fail|--inconclusive]";
            let gate = args.get(1).ok_or(USAGE)?;
            let summary = args.get(2).ok_or(USAGE)?;
            let mut outcome = cedian_workflow::Outcome::Pass;
            for flag in &args[3..] {
                match flag.as_str() {
                    "--fail" => outcome = cedian_workflow::Outcome::Fail,
                    "--inconclusive" => outcome = cedian_workflow::Outcome::Inconclusive,
                    _ => return Err(format!("unknown flag {flag:?}")),
                }
            }
            let mut state = workflow_store::load(workdir)?;
            // S2 exit: typed evidence has no router-log call behind it, so
            // it is unattributed and can never pass a required gate.
            let id = format!("e{}", state.evidence.len() + 1);
            let current = current_state(workdir);
            let item = cedian_workflow::Evidence::unattributed(
                &id,
                cedian_workflow::EvidenceKind::File,
                &[gate],
                summary,
                outcome,
            )
            .with_code_state(current.bind(&[]));
            state.attach(item).map_err(|e| e.to_string())?;
            workflow_store::save(workdir, &state)?;
            match state.gate_result(gate, &current) {
                Ok(r) => println!(
                    "evidence {id} → gate {gate:?}: {:?} ({})",
                    r.status, r.reason
                ),
                Err(e) => println!("evidence {id} attached ({}).", e),
            }
            Ok(())
        }
        Some("advance") => {
            let passed = !args[1..].contains(&"--fail".to_string());
            let mut state = workflow_store::load(workdir)?;
            state
                .advance(passed, &current_state(workdir))
                .map_err(|e| e.to_string())?;
            if !passed {
                println!("phase failed — workflow failed");
            }
            workflow_store::save(workdir, &state)?;
            render_workflow(&state, &current_state(workdir));
            Ok(())
        }
        Some("resume") => {
            let mut state = workflow_store::load(workdir)?;
            state.resume()?;
            workflow_store::save(workdir, &state)?;
            render_workflow(&state, &current_state(workdir));
            Ok(())
        }
        Some("complete") => {
            let mut state = workflow_store::load(workdir)?;
            match state.complete(&current_state(workdir)) {
                Ok(()) => {
                    workflow_store::save(workdir, &state)?;
                    println!("complete");
                    Ok(())
                }
                Err(missing) => {
                    workflow_store::save(workdir, &state)?;
                    Err(format!("blocked:\n  - {}", missing.join("\n  - ")))
                }
            }
        }
        _ => Err(
            "usage: cedian workflow <run|status|evidence|advance|resume|complete> …".to_string(),
        ),
    }
}

/// Browser commands (S4 headless surface — one-shot per invocation):
/// ```text
/// cedian browser open <url>                             # fresh Chrome → navigate → save head
/// cedian browser dom                                    # fresh Chrome on saved url → title, url, html
/// cedian browser shot [--attach <gate>] [--note <text>] # PNG into the state dir's shots/ (+ gate evidence)
/// cedian browser close                                  # clear head + sweep chrome-* profiles
/// cedian browser status                                 # saved head or `no browser session`
/// ```
///
/// Every action command spawns its own fresh Chrome (see `browser_store`):
/// the child dies with the invocation, so `dom`/`shot` re-navigate to the
/// saved url instead of reconnecting.
fn cmd_browser(workdir: &Path, args: &[String]) -> Result<(), String> {
    // One-shot per invocation: each arm spawns its own fresh Chrome, does
    // exactly one action, and saves the head. The `_proc` binding keeps the
    // child alive through the action (drops at scope end).
    match args.first().map(|s| s.as_str()) {
        Some("open") => {
            let url = args
                .get(1)
                .ok_or("usage: cedian browser open <url>")?
                .to_string();
            let (proc, session, ws_url) = browser_store::spawn_fresh(workdir, &url)?;
            let seq = session.current_seq();
            browser_store::save(
                workdir,
                &browser_store::BrowserHead {
                    port: proc.port,
                    ws_url,
                    url: url.clone(),
                    seq,
                },
            )?;
            println!("opened {url} (seq {seq})");
            Ok(())
        }
        Some("dom") => {
            let head = browser_store::load(workdir)?;
            let (_proc, session, _) = browser_store::spawn_fresh(workdir, &head.url)?;
            let dom = session.dom(2000).map_err(|e| e.to_string())?;
            println!("{} {}", dom.title, dom.url);
            println!("{}", dom.html);
            Ok(())
        }
        Some("shot") => {
            let mut attach: Option<String> = None;
            let mut note: Option<String> = None;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--attach" => {
                        let gate = args.get(i + 1).ok_or(
                            "usage: cedian browser shot [--attach <gate>] [--note <text>]",
                        )?;
                        attach = Some(gate.to_string());
                        i += 2;
                    }
                    "--note" => {
                        let text = args.get(i + 1).ok_or(
                            "usage: cedian browser shot [--attach <gate>] [--note <text>]",
                        )?;
                        note = Some(text.to_string());
                        i += 2;
                    }
                    flag => return Err(format!("unknown flag {flag:?}")),
                }
            }
            let head = browser_store::load(workdir)?;
            let (proc, session, ws_url) = browser_store::spawn_fresh(workdir, &head.url)?;
            let shot = session
                .screenshot(&state::dir(workdir)?.join("shots"))
                .map_err(|e| e.to_string())?;
            browser_store::save(
                workdir,
                &browser_store::BrowserHead {
                    port: proc.port,
                    ws_url,
                    url: head.url,
                    seq: session.current_seq(),
                },
            )?;
            println!(
                "shot {} frame {} seq {}",
                shot.path.display(),
                shot.frame_id,
                shot.seq
            );
            if let Some(gate) = attach {
                let mut state = workflow_store::load(workdir)?;
                let n = state.evidence.len() + 1;
                let id = format!("e{n}");
                let mut summary = format!(
                    "shot {} frame {} seq {}",
                    shot.path.display(),
                    shot.frame_id,
                    shot.seq
                );
                if let Some(text) = note {
                    summary.push(' ');
                    summary.push_str(&text);
                }
                // Row D: a cedian-owned headless capture, not a tracked
                // OMP call — unattributed, so no required gate counts it.
                let current = current_state(workdir);
                let item = cedian_workflow::Evidence::unattributed(
                    &id,
                    cedian_workflow::EvidenceKind::Screenshot,
                    &[gate.as_str()],
                    format!("{summary} [headless-capture]"),
                    cedian_workflow::Outcome::Pass,
                )
                .with_code_state(current.bind(&[]));
                state.attach(item).map_err(|e| e.to_string())?;
                workflow_store::save(workdir, &state)?;
                match state.gate_result(&gate, &current) {
                    Ok(r) => println!(
                        "evidence {id} → gate {gate:?}: {:?} ({})",
                        r.status, r.reason
                    ),
                    Err(e) => println!("evidence {id} attached ({e})."),
                }
            }
            Ok(())
        }
        Some("close") => {
            browser_store::clear(workdir);
            // Children died with their invocations; sweep leftover profiles.
            if let Ok(entries) =
                state::dir(workdir).and_then(|d| std::fs::read_dir(d).map_err(|e| e.to_string()))
            {
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy().starts_with("chrome-") {
                        let _ = std::fs::remove_dir_all(entry.path());
                    }
                }
            }
            println!("browser closed");
            Ok(())
        }
        Some("status") => match browser_store::load(workdir) {
            Ok(head) => {
                println!("{} (seq {})", head.url, head.seq);
                Ok(())
            }
            Err(_) => {
                println!("no browser session");
                Ok(())
            }
        },
        _ => Err("usage: cedian browser <open|dom|shot|close|status> …".to_string()),
    }
}

/// Read `--base <branch>` from `args[from..]`; defaults to `HEAD`.
fn worker_base(args: &[String], from: usize) -> Result<String, String> {
    let mut base = "HEAD".to_string();
    let mut i = from;
    while i < args.len() {
        match args[i].as_str() {
            "--base" => {
                base = args
                    .get(i + 1)
                    .cloned()
                    .ok_or("usage: cedian worker … [--base <branch>]")?;
                i += 2;
            }
            flag => return Err(format!("unknown flag {flag:?}")),
        }
    }
    Ok(base)
}

/// Worker commands (S5 headless surface — one-shot per invocation):
/// ```text
/// cedian worker spawn <id> <kind> <title> [--base <branch>]
/// cedian worker list
/// cedian worker steer <id> <note...>
/// cedian worker preview <id> [--base <branch>]
/// cedian worker merge-back <id> [--base <branch>]
/// cedian worker remove <id>
/// ```
///
/// `CEDIAN_WORKDIR` must be the repo root: the registry lives at
/// `workers.json` in its state dir (ADR-0044), worktrees at `<repo>/.worktrees/<id>`.
/// Base defaults to `HEAD` unless `--base <branch>` is given. `steer`
/// only records the note (status Running); the actual agent turn in the
/// worktree is a follow-up invocation.
fn cmd_worker(workdir: &Path, args: &[String]) -> Result<(), String> {
    match args.first().map(|s| s.as_str()) {
        Some("spawn") => {
            let usage = "usage: cedian worker spawn <id> <kind> <title> [--base B]";
            let id = args.get(1).ok_or(usage)?;
            let kind = args.get(2).ok_or(usage)?;
            let title = args.get(3).ok_or(usage)?;
            let base = worker_base(args, 4)?;
            let (mut reg, _) =
                cedian_worker::Registry::open(&state::dir(workdir)?).map_err(|e| e.to_string())?;
            let mut head = cedian_worker::spawn(workdir, id, &base).map_err(|e| e.to_string())?;
            head.status = cedian_worker::WorkerStatus::Running;
            head.task_title = title.clone();
            head.kind = kind.clone();
            let (worktree, branch) = (head.worktree.clone(), head.branch.clone());
            reg.insert(head).map_err(|e| e.to_string())?;
            reg.save(&state::dir(workdir)?).map_err(|e| e.to_string())?;
            println!("worker {id} → {worktree} (branch {branch})");
            Ok(())
        }
        Some("list") => {
            if args.len() > 1 {
                return Err("usage: cedian worker list".to_string());
            }
            let (reg, _) =
                cedian_worker::Registry::open(&state::dir(workdir)?).map_err(|e| e.to_string())?;
            let mut any = false;
            for head in reg.all() {
                any = true;
                let status = format!("{:?}", head.status).to_lowercase();
                println!(
                    "{} {} {} {}",
                    head.id, status, head.worktree, head.task_title
                );
            }
            if !any {
                println!("no workers");
            }
            Ok(())
        }
        Some("steer") => {
            let usage = "usage: cedian worker steer <id> <note...>";
            let id = args.get(1).ok_or(usage)?;
            if args.len() < 3 {
                return Err(usage.to_string());
            }
            let note = args[2..].join(" ");
            let (mut reg, _) =
                cedian_worker::Registry::open(&state::dir(workdir)?).map_err(|e| e.to_string())?;
            let head = reg
                .get(id)
                .cloned()
                .ok_or_else(|| cedian_worker::WorkerError::NoSuch(id.clone()).to_string())?;
            reg.set_status(id, cedian_worker::WorkerStatus::Running, note.clone())
                .map_err(|e| e.to_string())?;
            reg.save(&state::dir(workdir)?).map_err(|e| e.to_string())?;
            let wt = workdir.join(&head.worktree);
            println!("steer {id}: {note}");
            println!(
                "hint: run the turn with CEDIAN_WORKDIR={} cedian prompt ...",
                wt.display()
            );
            Ok(())
        }
        Some("preview") => {
            let id = args
                .get(1)
                .ok_or("usage: cedian worker preview <id> [--base <branch>]")?;
            let base = worker_base(args, 2)?;
            let (reg, _) =
                cedian_worker::Registry::open(&state::dir(workdir)?).map_err(|e| e.to_string())?;
            let head = reg
                .get(id)
                .cloned()
                .ok_or_else(|| cedian_worker::WorkerError::NoSuch(id.clone()).to_string())?;
            let plan =
                cedian_worker::merge_preview(workdir, &head, &base).map_err(|e| e.to_string())?;
            println!("clean:");
            for f in &plan.clean {
                println!("  {f}");
            }
            println!("conflicted(STALE):");
            for f in &plan.conflicted {
                println!("  {f}");
            }
            Ok(())
        }
        Some("merge-back") => {
            let id = args
                .get(1)
                .ok_or("usage: cedian worker merge-back <id> [--base <branch>]")?;
            let base = worker_base(args, 2)?;
            let (mut reg, _) =
                cedian_worker::Registry::open(&state::dir(workdir)?).map_err(|e| e.to_string())?;
            let head = reg
                .get(id)
                .cloned()
                .ok_or_else(|| cedian_worker::WorkerError::NoSuch(id.clone()).to_string())?;
            match cedian_worker::merge_back(workdir, &head, &base) {
                Ok(()) => {
                    reg.set_status(id, cedian_worker::WorkerStatus::Done, String::new())
                        .map_err(|e| e.to_string())?;
                    reg.save(&state::dir(workdir)?).map_err(|e| e.to_string())?;
                    println!("merged {} → {base}", head.branch);
                    Ok(())
                }
                Err(cedian_worker::WorkerError::Conflicted(files)) => {
                    println!("refused (STALE):");
                    for f in &files {
                        println!("  {f}");
                    }
                    Err(format!("refused (STALE): {}", files.join(", ")))
                }
                Err(e) => Err(e.to_string()),
            }
        }
        Some("remove") => {
            let id = args.get(1).ok_or("usage: cedian worker remove <id>")?;
            if args.len() > 2 {
                return Err("usage: cedian worker remove <id>".to_string());
            }
            let (mut reg, _) =
                cedian_worker::Registry::open(&state::dir(workdir)?).map_err(|e| e.to_string())?;
            let head = reg
                .get(id)
                .cloned()
                .ok_or_else(|| cedian_worker::WorkerError::NoSuch(id.clone()).to_string())?;
            cedian_worker::remove(workdir, &head).map_err(|e| e.to_string())?;
            reg.remove(id);
            reg.save(&state::dir(workdir)?).map_err(|e| e.to_string())?;
            println!("removed {id}");
            Ok(())
        }
        _ => Err("usage: cedian worker <spawn|list|steer|preview|merge-back|remove> …".into()),
    }
}

/// §51 render: title, kind · risk, phase checklist, gate states.
fn render_workflow(
    state: &cedian_workflow::WorkflowState,
    current: &cedian_workflow::CurrentState,
) {
    let kind = match state.task.kind {
        cedian_workflow::TaskKind::Investigation => "Investigation",
        cedian_workflow::TaskKind::BugFix => "Bug Fix",
        cedian_workflow::TaskKind::Feature => "Feature",
        cedian_workflow::TaskKind::Refactor => "Refactor",
        cedian_workflow::TaskKind::Performance => "Performance",
        cedian_workflow::TaskKind::Prototype => "Prototype",
    };
    let risk = match state.task.risk {
        cedian_workflow::Risk::Low => "Low",
        cedian_workflow::Risk::Medium => "Medium",
        cedian_workflow::Risk::High => "High",
    };
    println!("{}", state.task.title);
    println!("{kind} · {risk} risk · {:?}", state.status);
    println!();
    for ps in &state.phases {
        let glyph = match ps.status {
            cedian_workflow::PhaseStatus::Pending => "○",
            cedian_workflow::PhaseStatus::Running => "●",
            cedian_workflow::PhaseStatus::Passed => "✓",
            cedian_workflow::PhaseStatus::Failed => "✗",
            cedian_workflow::PhaseStatus::Blocked => "!",
            cedian_workflow::PhaseStatus::Skipped => "–",
        };
        if ps.status == cedian_workflow::PhaseStatus::Skipped {
            let reason = ps.skip_reason.as_deref().unwrap_or("conditional");
            println!("{glyph} {} (skipped: {reason})", ps.id);
            continue;
        }
        println!("{glyph} {}", ps.id);
    }
    println!();
    for (id, r) in state.all_gates(current) {
        let glyph = match r.status {
            cedian_workflow::GateStatus::Pending => "○",
            cedian_workflow::GateStatus::Passed => "✓",
            cedian_workflow::GateStatus::Failed => "✗",
            cedian_workflow::GateStatus::Blocked => "!",
            cedian_workflow::GateStatus::Skipped => "–",
        };
        let unverified = if r.unverified_origin {
            " [unverified-origin]"
        } else {
            ""
        };
        println!(
            "{glyph} gate {id}: {:?}{unverified} — {}",
            r.status, r.reason
        );
    }
    // Completion view (§55, ADR-0024): the last claim and every claim in it.
    if let Some(last) = &state.last_completion {
        println!();
        if last.accepted {
            println!("last completion: accepted");
        } else {
            println!("last completion: REFUSED");
            for m in &last.missing {
                println!("  - {m}");
            }
        }
        println!(
            "{}",
            cedian_workflow::ledger_lines(&last.claims).trim_start()
        );
    }
}

fn status_glyph(s: cedian_review::HunkStatus) -> &'static str {
    match s {
        cedian_review::HunkStatus::Pending => "pending",
        cedian_review::HunkStatus::Interrupted => "interrupted",
        cedian_review::HunkStatus::Unattributed => "unattributed",
        cedian_review::HunkStatus::Stale => "stale",
        cedian_review::HunkStatus::Accepted => "accepted",
        cedian_review::HunkStatus::Rejected => "rejected",
    }
}

/// Card status glyph for the turn render.
trait CardGlyph {
    fn status_glyph(&self) -> &'static str;
}

impl CardGlyph for cedian_agent_ui::ToolCard {
    fn status_glyph(&self) -> &'static str {
        match self.status {
            cedian_agent_ui::ToolCardStatus::Running => "…",
            cedian_agent_ui::ToolCardStatus::Done => "✓",
            cedian_agent_ui::ToolCardStatus::Error => "✗",
            cedian_agent_ui::ToolCardStatus::Interrupted => "!",
        }
    }
}

/// Re-export for tests.
use cedian_workspace::Version as _Version;
#[allow(unused)]
fn _keep_version(_: _Version) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_goes_stale_on_lockfile_and_large_file_changes() {
        let dir = std::env::temp_dir().join(format!("cedian-code-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.lock"), "serde 1.0.0").unwrap();
        let big = vec![b'x'; workspace_files::MAX_FILE_BYTES as usize + 1];
        std::fs::write(dir.join("fixture.dat"), &big).unwrap();

        let bound = current_state(&dir).bind(&[]);
        std::fs::write(dir.join("Cargo.lock"), "serde 1.0.1").unwrap();
        assert!(
            current_state(&dir).stale_reason(&bound).is_some(),
            "a lockfile bump makes tree-bound evidence stale"
        );

        let bound = current_state(&dir).bind(&[]);
        let mut bigger = big;
        bigger.push(b'y');
        std::fs::write(dir.join("fixture.dat"), &bigger).unwrap();
        assert!(
            current_state(&dir).stale_reason(&bound).is_some(),
            "a change to a file over the buffer size cap makes tree-bound evidence stale"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opt_in_hands_approvals_to_omp_but_keeps_denies() {
        use cedian_omp::Approvals;
        use cedian_shell::{Policy, Verdict};
        let mut settings = cedian_shell::Settings::default();
        let default = spawn_policy(&settings, Policy::Cedian);
        assert_eq!(default.approvals, Approvals::Cedian(ApprovalMode::Write));
        assert_eq!(
            spawn_policy(&settings, Policy::Omp).approvals,
            Approvals::Omp
        );
        settings.permissions.project_write = Verdict::Deny;
        let opted = spawn_policy(&settings, Policy::Omp);
        assert_eq!(opted.approvals, Approvals::Omp);
        assert_eq!(opted.tool_policies.get("write"), Some(&ToolPolicy::Deny));
        assert_eq!(
            spawn_policy(&settings, Policy::Cedian).approvals,
            Approvals::Cedian(ApprovalMode::AlwaysAsk)
        );
    }

    #[test]
    fn every_reporting_host_tool_is_a_channel_call() {
        let mut settings = cedian_shell::Settings::default();
        let names = host_tool_names(&settings);
        assert!(names.contains(&cedian_worker::WORKTREE_REQUEST_TOOL));
        for name in names
            .iter()
            .filter(|n| **n != cedian_workspace::APPLY_EDIT_TOOL)
        {
            assert!(cedian_workflow::is_channel_call(name, ""), "{name}");
        }
        settings.permissions.project_write = cedian_shell::Verdict::Deny;
        assert!(!host_tool_names(&settings).contains(&cedian_worker::WORKTREE_REQUEST_TOOL));
    }

    #[test]
    fn refused_claim_blocks_at_turn_end_and_resume_unblocks() {
        use cedian_workflow::{CompletionAttempt, TaskKind, TaskProfile, WorkflowStatus};
        let d = std::env::temp_dir().join(format!("cedian-u5-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        end_workflow_turn(&d).unwrap(); // fast lane: no workflow, nothing to do
        assert!(!workflow_store::exists(&d));
        let mut state =
            cedian_workflow::WorkflowState::start(TaskProfile::new("t", TaskKind::BugFix)).unwrap();
        state.last_completion = Some(CompletionAttempt {
            claims: vec![],
            accepted: false,
            missing: vec!["required gate \"verify\"".into()],
            turn_ended: false,
        });
        workflow_store::save(&d, &state).unwrap();
        end_workflow_turn(&d).unwrap();
        assert_eq!(
            workflow_store::load(&d).unwrap().status,
            WorkflowStatus::Blocked
        );
        cmd_workflow(
            &d,
            &cedian_shell::Settings::default(),
            &["resume".to_string()],
        )
        .unwrap();
        assert_eq!(
            workflow_store::load(&d).unwrap().status,
            WorkflowStatus::Running
        );
        assert!(
            cmd_workflow(
                &d,
                &cedian_shell::Settings::default(),
                &["resume".to_string()]
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The verify-notes profile skill (test fixture copy) only uses keys and
    /// enum values the channel accepts, names every stage, and has a feature
    /// map cedian can read (ADR-0025).
    #[test]
    fn verify_notes_skill_matches_the_channel_schema() {
        let skill = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/skills/verify-notes/SKILL.md"
        ))
        .unwrap();
        assert!(skill.contains("name: verify-notes"));
        assert_eq!(cedian_workflow::feature_map(&skill), ["add-note"]);
        let update = cedian_workflow::update_parameters();
        let props = update["properties"].as_object().unwrap();
        let mut stages = Vec::new();
        for block in skill.split("```json").skip(1) {
            let body = block.split("```").next().unwrap();
            let obj: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(body.trim()).unwrap_or_else(|e| panic!("{body}: {e}"));
            for (key, value) in &obj {
                let prop = props
                    .get(key)
                    .unwrap_or_else(|| panic!("unknown key {key}"));
                if let (Some(allowed), Some(v)) = (prop.get("enum"), value.as_str()) {
                    assert!(
                        v.starts_with('<') || allowed.as_array().unwrap().iter().any(|a| a == v),
                        "{key}={v}"
                    );
                }
            }
            if let Some(stage) = obj.get("stage").and_then(|v| v.as_str()) {
                stages.push(stage.to_string());
            }
        }
        assert_eq!(stages, ["launch", "doctor", "drive", "evidence", "cleanup"]);
    }

    /// U8: the bug-fix playbook skill (a test fixture copy — cedian never
    /// writes the user's `.omp/`, §77) only uses tool names, ops, keys and
    /// enum values the channel actually accepts, and the gates it reports
    /// are the bug_fix playbook's.
    #[test]
    fn bug_fix_skill_matches_the_channel_schema() {
        use serde_json::Value;
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/skills/bug-fix/SKILL.md"
        );
        let skill = std::fs::read_to_string(path).unwrap();
        let front = skill.split("---").nth(1).expect("frontmatter");
        assert!(front.contains("name: bug-fix"), "{front}");
        assert!(front.contains("description: "), "{front}");

        for word in skill.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            if word.starts_with("cedian_") {
                assert!(
                    cedian_workflow::CHANNEL_TOOLS.contains(&word),
                    "unknown tool {word}"
                );
            }
        }
        let check = |obj: &serde_json::Map<String, Value>,
                     schema: &serde_json::Map<String, Value>,
                     ctx: &str| {
            let props = schema["properties"].as_object().unwrap();
            for (key, value) in obj {
                let prop = props
                    .get(key)
                    .unwrap_or_else(|| panic!("{ctx}: unknown key {key:?}"));
                if let (Some(allowed), Some(v)) = (prop.get("enum"), value.as_str()) {
                    if !v.starts_with('<') {
                        assert!(
                            allowed.as_array().unwrap().iter().any(|a| a == v),
                            "{ctx}: {key}={v:?} not in {allowed}"
                        );
                    }
                }
            }
        };
        let update = cedian_workflow::update_parameters();
        let complete = cedian_workflow::complete_parameters();
        let bug_fix = cedian_workflow::Playbook::bug_fix();
        let mut ops = Vec::new();
        for block in skill.split("```json").skip(1) {
            let body = block.split("```").next().unwrap();
            let obj: serde_json::Map<String, Value> = serde_json::from_str(body.trim())
                .unwrap_or_else(|e| panic!("bad JSON {body}: {e}"));
            if let Some(op) = obj.get("op").and_then(Value::as_str) {
                check(&obj, &update, op);
                ops.push(op.to_string());
                if let Some(gate) = obj.get("gate").and_then(Value::as_str) {
                    assert!(bug_fix.gates.iter().any(|g| g.id == gate), "no gate {gate}");
                }
            } else {
                check(&obj, &complete, "cedian_complete");
                let claim_schema = complete["properties"]["claims"]["items"]
                    .as_object()
                    .unwrap();
                for claim in obj["claims"].as_array().unwrap() {
                    check(claim.as_object().unwrap(), claim_schema, "claim");
                }
            }
        }
        for op in ["start", "evidence", "advance"] {
            assert!(ops.iter().any(|o| o == op), "skill never shows op {op}");
        }
    }
}
