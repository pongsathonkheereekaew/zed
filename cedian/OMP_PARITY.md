# OMP feature parity

The one home for **per-feature OMP coverage** ([ADR-0034](decisions/0034-identity-and-omp-parity.md)). Slice status lives in [`../README.md`](../README.md); this file says, for every OMP capability, where it surfaces in cedian.

- **Pinned OMP:** `vendor/omp-revision.json` (RPC client generated from it: `vendor/omp-rpc/src/wire.rs`).
- **Statuses:** `native` (GPUI surface) · `headless` (wired in cedian crates; GPUI surface at S9) · `planned Sx` · `gated ADR-xxxx` (off by default, opt-in named) · `upstream-blocked` (upstream PR link).
- **No permanent cuts.** Removing a row needs an ADR.
- **Enforced:** the P7 test (`crates/cedian_omp/tests/omp_parity.rs`) fails if a command, server notification or UI request in `wire.rs` is missing here. Names are the wire names, exactly as OMP sends them. On every pin bump, regenerate `wire.rs`, add rows, and review the tool and config tables by hand.
- Superseded history: `spike/CAPABILITY_TABLE.md` (OMP 18.6.1, frozen).

## RPC commands (65)

| Command(s) | cedian surface | Status |
|---|---|---|
| `negotiate_protocol` | runtime handshake (v2) | headless |
| `prompt`, `abort` | composer send / stop | headless |
| `steer` | steer while a turn runs | headless |
| `follow_up`, `abort_and_prompt`, `abort_and_restore_queue` | composer queue actions | planned S9 |
| `remove_queued_message`, `promote_queued_message` | queued-message chips | planned S9 |
| `set_steering_mode`, `set_follow_up_mode`, `set_interrupt_mode` | composer settings | planned S9 |
| `get_state` | runtime state / status line | headless |
| `new_session`, `open_session` | new task / resume task | headless |
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
| `goal` | Goal mode ([ADR-0014](decisions/0014-agent-modes.md)) | planned S9 |
| `set_todos` | todo list in Workflow UI | planned S2 |
| `set_host_tools`, `set_host_uri_schemes` | cedian host tools, `cedian://` | headless |
| `set_ask_dialog` | native ask dialogs | headless |
| `set_event_filter` | router subscription | planned S9 |
| `get_available_commands` | slash commands in palette and composer | planned S9 |
| `set_subagent_subscription`, `get_subagents`, `get_subagent_messages` | subagent tree | planned S5 |
| `steer_subagent`, `cancel_subagent` | subagent steer / cancel | planned S5 |
| `bash`, `abort_bash` | user-run shell command in agent context | planned S9 |
| `btw`, `btw_cancel`, `get_btw_history` | side question without disturbing the turn | planned S9 |
| `predict_word`, `predict_word_feedback` | composer word prediction (not editor completion, [ADR-0023](decisions/0023-product-scope.md)) | planned S9 |
| `live_start`, `live_stop`, `live_mute` | live (voice) session | planned S9 |

## Agent events (31)

| Event(s) | cedian surface | Status |
|---|---|---|
| `agent_start`, `agent_end`, `turn_start`, `turn_end` | turn lifecycle | headless |
| `message_start`, `message_update`, `message_end` | streaming thread | headless |
| `tool_execution_start`, `tool_execution_update`, `tool_stream_update`, `tool_execution_end` | tool cards, edit import, provenance | headless |
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
| `extension_ui_request` | carries the UI requests below | headless |
| `extension_error` | extension error toast | planned S9 |
| `available_commands_update` | slash-command palette refresh | planned S9 |
| `subagent_lifecycle`, `subagent_progress`, `subagent_event` | subagent tree | planned S5 |
| `live_phase`, `live_levels`, `live_transcript`, `live_end` | live (voice) session | planned S9 |
| `btw_delta`, `btw_record` | side-question stream and history | planned S9 |
| `command_output` | output of user-run `bash` | planned S9 |
| `session_info_update` | session manager (name, metadata) | planned S9 |
| `config_update` | settings refresh, live in both directions ([ADR-0040](decisions/0040-omp-settings-mirror-omp-config.md)) | planned S9 |

## UI requests (12)

| Request(s) | cedian surface | Status |
|---|---|---|
| `select`, `confirm`, `input`, `editor`, `ask`, `cancel` | native dialogs ([§63](ARCHITECTURE.md)) | headless (fail-closed answers) |
| `notify` | toast | planned S9 |
| `setStatus`, `setWidget`, `setTitle` | status line, panel widget, window title | planned S9 |
| `set_editor_text` | composer text | planned S9 |
| `open_url` | open in cedian browser or system browser | planned S9 |

## Tools (hand-reviewed on pin bump)

| Tool | cedian surface | Status |
|---|---|---|
| `read`, `grep`/`glob`/`find`, `ast_grep` | tool cards | headless |
| `edit`, `write`, `ast_edit` | agent transactions, review, undo ([ADR-0027](decisions/0027-zero-omp-fork.md)) | headless (disk), native at S9 |
| `bash`, `eval` | tool cards; prompts per approval mode | headless |
| `lsp`, `debug` | backed by Zed LSP/DAP via `cedian://` | headless stand-in (row B); native S9 |
| `task`, `hub`/`wait`, `vibe_spawn`/`vibe_send` | subagent tree | planned S5 |
| `todo` | Workflow UI | planned S2 |
| `ask` | native dialog | headless |
| `browser` | shared Chromium + browser pane | headless stand-in (row D); native S4/S9 |
| `computer` (`eval` prelude) | OMP prelude by opt-in; CUA driver later | headless by opt-in ([ADR-0035](decisions/0035-omp-native-approval-opt-in.md), P8): off in the default profile; under `policy = "omp"` OMP's config decides and the badge shows it |
| `gh` | PR workspace | planned S6 |
| MCP and extension tools | generic tool card ([§68](ARCHITECTURE.md)) | headless |

## Launch options and config (hand-reviewed on pin bump)

| Feature | cedian surface | Status |
|---|---|---|
| approval modes (`always-ask`, `write`, `yolo`) | default `write`; `always-ask` for reviewers (S3); OMP's own mode by opt-in | headless by opt-in ([ADR-0035](decisions/0035-omp-native-approval-opt-in.md), P8): `policy = "omp"` passes no mode; badge, `approved by OMP` card label, `.cedian/audit.jsonl` rows |
| `omp config get <key> --json` | OMP policy badge (effective `tools.approvalMode`, `computer.enabled`); `tools.approval` read before every default-profile spawn to pin unnamed allows ([ADR-0039](decisions/0039-reviewer-is-a-host-spawned-omp-process.md)) | headless (P8, S3 U1) |
| `--model` (launch-time) | the reviewer's `[review] model` from `cedian.toml` ([ADR-0039](decisions/0039-reviewer-is-a-host-spawned-omp-process.md)) | headless (S3): reviewer processes only |
| running under `sandbox-exec` | the reviewer's generated Seatbelt profile (workspace unwritable, exec allow-list) | headless (S3): reviewer processes only |
| Plan mode (launch-time) | separate plan runtime ([ADR-0014](decisions/0014-agent-modes.md)) | planned S9 |
| `--profile` | per-workspace OMP profile in settings | planned S9 |
| skills, rules, `AGENTS.md`, agents (`.omp/`) | used as-is; listed read-only in settings ([§77](ARCHITECTURE.md)) | headless (used); listing planned S9 |
| MCP servers, extensions | used as-is; listed read-only in settings | headless (used); listing planned S9 |
| memory | used as-is | headless |
| provider/model routing | model picker | planned S9 |
| model roles (`modelRoles`, `modelRoleStorage`, `--smol`/`--slow`/`--plan`) | settings page "Model roles"; reviewer role ([ADR-0039](decisions/0039-model-roles-and-independent-review.md)) | planned S9 (page), S3 (reviewer role) |
