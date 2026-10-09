# OMP feature parity

The one home for **per-feature OMP coverage** ([ADR-0034](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0034-identity-and-omp-parity.md)). Slice status lives in [`../README.md`](https://github.com/pongsathonkheereekaew/cedian/blob/main/README.md); this file says, for every OMP capability, where it surfaces in cedian.

- **Pinned OMP:** `vendor/omp-revision.json` (RPC client generated from it: `vendor/omp-rpc/src/wire.rs`).
- **Statuses:** `native` (GPUI surface) · `headless` (wired in cedian crates; GPUI surface at S9) · `planned Sx` · `gated ADR-xxxx` (off by default, opt-in named) · `upstream-blocked` (upstream PR link).
- **No permanent cuts.** Removing a row needs an ADR.
- **Enforced:** the P7 test (`crates/cedian_omp/tests/omp_parity.rs`) fails if a command, server notification or UI request in `wire.rs` is missing here. Names are the wire names, exactly as OMP sends them. On every pin bump, regenerate `wire.rs`, add rows, and review the tool and config tables by hand.
- Superseded history: `spike/CAPABILITY_TABLE.md` (OMP 18.6.1, frozen).

## RPC commands (65)

| Command(s) | cedian surface | Status |
|---|---|---|
| `negotiate_protocol` | runtime handshake (v2) | headless |
| `prompt`, `abort` | composer send (images pasted into the composer go as `images`) / Stop button while a turn streams (Stop first cancels any open dialog, audited `abstain`); an audit write failure aborts the turn once (S9 U4) | native |
| `steer` | the Steer button while a turn streams sends the composer text into the running turn (S9 U8, ADR-0050 decision 3); `cedian shell` `steer` headless | native |
| `follow_up` | Send (Enter) while a turn streams queues the text after the turn; the chip is OMP's latest `queue_update` (S9 U8, ADR-0050 decision 3) | native |
| `abort_and_prompt` | composer queue actions | planned S9 |
| `remove_queued_message` | Stop takes each queued steer and follow-up back with it before `abort` (OMP's `abort` keeps the queue and a kept steer starts a new run) and puts the text back in the composer, oldest first (S9 U8). OMP 18.6.1 has no `abort_and_restore_queue` | native (S9 U8) |
| `promote_queued_message` | queued-message chips | planned S9 |
| `set_steering_mode`, `set_follow_up_mode`, `set_interrupt_mode` | composer settings | planned S9 |
| `get_state` | runtime state / status line | headless |
| `new_session`, `open_session` | new task / resume task; the app opens the workspace's session on launch and Restart adopts it (S9 U3). OMP lets a second process resume a live session; the app does not: after a resume and before each prompt it lists the processes holding the session file or OMP's owner lease (`~/.omp/run/session-owners/<id>.lock`, `lsof`), and any but its own OMP refuses the session with "Start a new session" (`new_session`) and "Retry" (ADR-0040 decision 5, S9 U4). Gap: a process that resumed but has not written yet holds neither file | native (`open_session` U3, one-driver check and `new_session` U4) |
| `switch_session`, `set_session_name` | session manager | planned S9 |
| `branch`, `fork`, `get_branch_messages`, `get_tree` | thread tree: branch, fork, checkpoints | planned S9 |
| `get_entries`, `get_messages`, `get_messages_page`, `get_last_assistant_text` | thread history and paging | planned S9 |
| `handoff` | hand off to a new session with summary | planned S9 |
| `export_html` | export thread | planned S9 |
| `get_session_stats` | usage / cost meter | planned S9 |
| `set_model`, `cycle_model`, `get_available_models` | model picker | `set_model` headless; rest planned S9 |
| `set_thinking_level`, `cycle_thinking_level`, `get_available_thinking_levels` | thinking-level picker | planned S9 |
| `set_fast_mode`, `set_slow_mode` | speed toggles | planned S9 |
| `get_login_providers`, `login` | onboarding / account settings | planned S9 |
| `compact`, `set_auto_compaction` | context meter + compact action | planned S9 |
| `set_cache_warming`, `set_auto_retry`, `abort_retry` | runtime settings, retry banner | planned S9 |
| `goal` | Goal mode ([ADR-0014](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0014-agent-modes.md)) | planned S9 |
| `set_todos` | todo list in Workflow UI | planned S2 |
| `set_host_tools`, `set_host_uri_schemes` | headless (CLI): the host tool `cedian_apply_edit` and `cedian://`; `cedian_apply_edit` takes `expected_version` as the buffer's version token (`0` for a buffer no edit has touched, else `replica.seq` pairs), and a malformed or outdated token refuses the edit. The app registers `cedian_worktree_request` (only when `project_write = "allow"`: it makes a tree and a branch with no dialog, so under `ask` it is not registered until an in-app approval exists (follow-up); the request must carry the [ADR-0033](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0033-worker-brief-contract.md) brief, `goal`, `scope.write`, `acceptance`, `verify`, `timebox_min`, or it is refused naming the missing fields with no tree; the brief is stored as revision 1 beside the registry row, S9 U8) and the `cedian` URI scheme (S9 U6): `buffer`, `selection`, `active-file`, `diagnostics`, `open-editors` answered from Zed (unsaved text included), each read in its own task with a 20 s bound, a cancelled read skipped; a private file (`private_files`) or a path that resolves outside the folder is refused. The app also registers the workflow channel, `cedian_workflow_update` and `cedian_complete`, under either policy (they write no workspace file): its store is `workflow.json` in the workspace's state dir, evidence binds to OMP's finished calls in the router log, a claim refused in a turn blocks the workflow when OMP settles, and the panel shows its phases, gates with their reasons, evidence outcomes (inconclusive never as a pass) and the last claims ledger, with Resume on a blocked one ([ADR-0055](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0055-rigor-visible-in-the-app.md); S9 U9) | native: `set_host_uri_schemes` (S9 U6), `set_host_tools` with `cedian_worktree_request` (S9 U8) and the workflow channel (S9 U9); headless: the CLI's other host tools |
| `set_ask_dialog` | native ask dialogs | headless |
| `set_event_filter` | router subscription | planned S9 |
| `get_available_commands` | slash commands in palette and composer | planned S9 |
| `set_subagent_subscription` | the app's link subscribes at level `progress` when OMP starts ([ADR-0050](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0050-workers-are-omp-subagents.md); S9 U8) | native |
| `get_subagents`, `get_subagent_messages` | subagent snapshot / a subagent's transcript on demand | planned S9 |
| `steer_subagent`, `cancel_subagent` | Steer (text box) and Cancel on each running subagent row, sent off the UI thread; a refused steer shows on the row; `cancelled: false` shows "already ended"; each Cancel is an audit gate row (S9 U8, ADR-0050 decision 4) | native |
| `bash`, `abort_bash` | user-run shell command in agent context | planned S9 |
| `btw`, `btw_cancel`, `get_btw_history` | side question without disturbing the turn | planned S9 |
| `predict_word`, `predict_word_feedback` | composer word prediction (not editor completion, [ADR-0023](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0023-product-scope.md)) | planned S9 |
| `live_start`, `live_stop`, `live_mute` | live (voice) session | planned S9 |

## Agent events (31)

| Event(s) | cedian surface | Status |
|---|---|---|
| `agent_start`, `agent_end`, `turn_start`, `turn_end` | turn lifecycle | headless |
| `message_start`, `message_update`, `message_end` | streaming thread | headless |
| `tool_execution_start`, `tool_execution_update`, `tool_stream_update`, `tool_execution_end` | tool cards; `tool_execution_start` of `edit`/`write`/`ast_edit` names the files the call writes (the app opens the ones not open and marks every open buffer before the write; an `xd://` path is OMP's mount of a host tool, never a file), `tool_execution_end` imports the write as one buffer transaction attributed to the `toolCallId` in the task's review (S9 U5). OMP does not wait for the host, so its write can land before cedian reads the file: an `edit` result's `details.oldText` (per file, or in `perFileResults`; the whole file before the call, absent for a new file) is then the baseline; when OMP pruned it (`snapshotsPruned`, past 32 KiB) or the tool reports none (`write`), a file that reads unchanged is listed as not reviewable, never Unchanged (S9 U9, ADR-0055) | native (S9 U5); headless (cards) |
| `auto_compaction_start`, `auto_compaction_end` | compaction notice | planned S9 |
| `auto_retry_start`, `auto_retry_end`, `retry_fallback_applied`, `retry_fallback_succeeded` | retry / fallback banner | planned S9 |
| `cache_warming_start`, `cache_warming_end` | status line | planned S9 |
| `model_changed`, `thinking_level_changed` | model picker state | planned S9 |
| `config_warnings_changed` | settings warnings | planned S9 |
| `advisor_cost_changed`, `advisor_yielded` | usage meter | planned S9 |
| `ttsr_triggered` | thread notice | planned S9 |
| `todo_reminder`, `todo_auto_clear` | Workflow UI todos | planned S2 |
| `goal_updated` | Goal mode | planned S9 |
| `queue_update` | queued-message chips | planned S9 |
| `irc_message` | inter-agent message in subagent view | planned S5 |
| `notice` | toast | planned S9 |

## Other server notifications (19)

Frames OMP sends besides the agent events above.

| Notification(s) | cedian surface | Status |
|---|---|---|
| `ready`, `rpc_frame_error` | runtime handshake; protocol error banner (fail safe, §5) | headless |
| `prompt_result`, `session_settled` | turn completion state | headless |
| `extension_ui_request` | carries the UI requests below | native (dialogs, S9 U4); see rows below |
| `extension_error` | extension error toast | planned S9 |
| `available_commands_update` | slash-command palette refresh | planned S9 |
| `subagent_lifecycle`, `subagent_progress` | subagent rows under the `task` tool card that started them (`parentToolCallId`), with status running / completed / failed / aborted (S9 U8) | native |
| `subagent_event` | not subscribed: level `events` streams every subagent token ([ADR-0050](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0050-workers-are-omp-subagents.md) decision 1); transcripts come from `get_subagent_messages` | unused by decision |
| `live_phase`, `live_levels`, `live_transcript`, `live_end` | live (voice) session | planned S9 |
| `btw_delta`, `btw_record` | side-question stream and history | planned S9 |
| `command_output` | output of user-run `bash` | planned S9 |
| `session_info_update` | session manager (name, metadata) | planned S9 |
| `config_update` | the settings page re-reads on it ([ADR-0040](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0040-omp-settings-mirror-omp-config.md)); it carries only model and thinking level, so file changes are watched instead ([ADR-0045](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0045-omp-settings-live-by-watching-config-sources.md)) | native (S9 U3a) |

## UI requests (12)

| Request(s) | cedian surface | Status |
|---|---|---|
| `select`, `confirm`, `input`, `editor`, `ask`, `cancel` | native dialogs ([§63](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/ARCHITECTURE.md)): the app panel shows each open dialog (one button per `select` option, Yes/No, a text box, `ask` questions with options and free text, Dismiss on all) and answers it; `audit.jsonl` gets a gate row `answered_by: user`; OMP's `cancel` removes the dialog; a dialog unanswered for 5 minutes, or open at Stop or restart, gets a cancel reply (`timedOut` on expiry) and an `abstain` row by cedian (§63); one that expires while a workflow runs blocks its current phase, shows the escalation with Resume and is a `continue_escalated` correction row, and outside a workflow it is a notice only (§54, S9 U9) | native (S9 U4); headless runs answer fail-closed |
| `notify` | toast | planned S9 |
| `setStatus`, `setWidget`, `setTitle` | status line, panel widget, window title | planned S9 |
| `set_editor_text` | composer text | planned S9 |
| `open_url` | open in the app's owned Chromium (ADR-0049); headless browser use is OMP's own | planned S9 |

## Tools (hand-reviewed on pin bump)

| Tool | cedian surface | Status |
|---|---|---|
| `read`, `grep`/`glob`/`find`, `ast_grep` | tool cards | headless |
| `edit`, `write`, `ast_edit` | one agent transaction per call on the Zed buffer, native undo; Review Changes per task with Accept/Reject per hunk, Accept all (skips STALE), Revert turn; a hunk the user edited after the agent is STALE and never rejected; a write over unsaved edits is refused and the file shows STALE with the reason ([ADR-0006](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0006-review-baseline-provenance-precedence.md), [ADR-0027](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0027-zero-omp-fork.md)) | native (S9 U5); the headless CLI review commands are deleted (stand-in G) |
| `bash`, `eval` | tool cards (the summary is the output, without OMP's `Wall time:` footer); prompts per approval mode, answered in the app's approval dialog (S9 U4; a real OMP 18.6.1 approval recorded and replayed in the app: `cedian_panel/tests/live_approval.rs`) | headless (cards); native (approvals) |
| `lsp` | OMP's own tool in the app and headless (`--no-lsp` is not set). In the app the agent also reads Zed's language servers through `cedian://definitions`, `references`, `symbols`: the servers the person already runs, results in a private file or outside the folder dropped, columns UTF-16; headless those reads answer an error naming OMP's `lsp` tool ([ADR-0048](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0048-one-lsp-zeds-staged.md)) | native (S9 U6): `cedian://` reads; OMP's tool as upstream ships it |
| `debug` | OMP's own tool as upstream ships it; nothing in cedian uses DAP (stand-in B's `cedian_dap` is deleted, [ADR-0048](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0048-one-lsp-zeds-staged.md)) | headless: OMP's tool |
| `task` | its subagents are rows under the `task` tool card, with Steer and Cancel ([ADR-0050](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0050-workers-are-omp-subagents.md); S9 U8) | native |
| `hub`/`wait`, `vibe_spawn`/`vibe_send` | not rendered as subagents yet | planned |
| `todo` | Workflow UI | planned S2 |
| `ask` | native dialog | native (S9 U4) |
| `browser` (tool, and the `browser` Puppeteer prelude in `eval`) | OMP attaches to the one Chromium the app owns per workspace (its own profile in the state dir, started on OMP's first connection or the panel's "Open browser"), through cedian's loopback endpoint; the panel shows the latest capture inline with its frame sequence, console and network; evidence from an earlier frame reads `stale-frame`; the person's input in the window during a turn holds OMP's next browser message until they let the agent continue ([ADR-0049](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0049-one-chromium-owned-by-the-app.md)). Gaps: captures are cedian's (the panel's Capture), not attributed to OMP's call; input is watched on the first page only; no inline screencast | native (S9 U7); stand-in D not yet deleted |
| `computer` (`eval` prelude) | OMP prelude by opt-in; CUA driver later | headless by opt-in ([ADR-0035](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0035-omp-native-approval-opt-in.md), P8): off in the default profile; under `policy = "omp"` OMP's config decides and the badge shows it |
| `gh` | PR workspace | planned S6 |
| MCP and extension tools | generic tool card ([§68](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/ARCHITECTURE.md)) | headless |

## Launch options and config (hand-reviewed on pin bump)

| Feature | cedian surface | Status |
|---|---|---|
| approval modes (`always-ask`, `write`, `yolo`) | default `write`; `always-ask` for reviewers (S3); OMP's own mode by opt-in | headless by opt-in ([ADR-0035](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0035-omp-native-approval-opt-in.md), P8): `policy = "omp"` passes no mode; badge, `approved by OMP` card label, `audit.jsonl` rows in the state dir ([ADR-0044](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0044-cedian-state-outside-the-workspace.md)) |
| `omp config list`, `set`, `reset`, `path` (`--json`) | the OMP settings page: every key with its value and derived layer, simple types edited in place, `modelRoles` by role, `overriddenBy` shown, OMP's refusal shown; the global and project `config.yml` are watched ([ADR-0045](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0045-omp-settings-live-by-watching-config-sources.md)) | native (S9 U3a) |
| `PI_CODING_AGENT_DIR`, `OMP_PROFILE` | passed through the spawn profile, so cedian and the CLI read one config ([ADR-0045](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0045-omp-settings-live-by-watching-config-sources.md)) | native (S9 U3a) |
| `omp config get <key> --json` | OMP policy badge (effective `tools.approvalMode`, `computer.enabled`); `tools.approval` read before every default-profile spawn to pin unnamed allows ([ADR-0041](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0041-reviewer-is-a-host-spawned-omp-process.md)) | headless (P8, S3 U1) |
| `--model` (launch-time) | the reviewer runs on its `review` role, resolved from OMP's `modelRoles` in a cedian-owned directory; a same-model review is `inconclusive` ([ADR-0039](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0039-model-roles-and-independent-review.md), [ADR-0041](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0041-reviewer-is-a-host-spawned-omp-process.md)) | headless (S3): reviewer processes only |
| running under `sandbox-exec` | the reviewer's generated Seatbelt profile: writes only its per-review run dir, workspace unwritable, credential reads denied, exec allow-list ([ADR-0043](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0043-reviewer-sandbox-private-state-no-credentials.md)) | headless (S3): reviewer processes only |
| `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `TMPDIR` (OMP's state, cache and temp roots) | pointed into the reviewer's run dir, so OMP's run, log and daemon files stay inside it ([ADR-0043](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0043-reviewer-sandbox-private-state-no-credentials.md)) | headless (S3): reviewer processes only |
| `--tools`, `--no-extensions`, `--no-skills`, `--no-lsp`, `mcp.enableProjectConfig` | the reviewer's fixed tool set: `read,grep,glob,bash`, no extensions, skills, language servers or project MCP servers ([ADR-0043](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0043-reviewer-sandbox-private-state-no-credentials.md)) | headless (S3): reviewer processes only |
| `browser.cdpUrl` | the overlay sets it to the panel's endpoint, `http://127.0.0.1:<port>`, under every policy, `policy = "omp"` included, so OMP never launches its own browser; not for a reviewer, whose tool set has no browser (ADR-0043) ([ADR-0049](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0049-one-chromium-owned-by-the-app.md)) | native (S9 U7) |
| `task.isolation.enabled` | the overlay pins `false` under the default profile and for reviewers, so OMP's `task` never makes its own worktree; worktrees are cedian's (ADR-0009, [ADR-0050](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0050-workers-are-omp-subagents.md) decision 2); under `policy = "omp"` the user's OMP config decides and the badge says so (S9 U8) | pinned |
| `browser.relay` (and `PI_BROWSER_RELAY`) | the overlay sets `false` under every policy, so OMP never drives the person's own Chrome; the env var is not on the spawn allow-list ([ADR-0049](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0049-one-chromium-owned-by-the-app.md)) | native (S9 U7) |
| Plan mode (launch-time) | separate plan runtime ([ADR-0014](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0014-agent-modes.md)) | planned S9 |
| `--profile` | per-workspace OMP profile in settings | planned S9 |
| skills, rules, `AGENTS.md`, agents (`.omp/`) | used as-is; listed read-only in settings ([§77](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/ARCHITECTURE.md)) | headless (used); listing planned S9 |
| MCP servers, extensions | used as-is; listed read-only in settings | headless (used); listing planned S9 |
| memory | used as-is | headless |
| provider/model routing | model picker | planned S9 |
| model roles (`modelRoles`, `modelRoleStorage`, `--smol`/`--slow`/`--plan`) | settings page "Model roles"; reviewer role ([ADR-0039](https://github.com/pongsathonkheereekaew/cedian/blob/main/docs/decisions/0039-model-roles-and-independent-review.md)) | planned S9 (page), S3 (reviewer role) |
