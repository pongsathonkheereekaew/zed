#!/usr/bin/env bash
# S2 speed benchmark harness (ADR-0026, ADR-0037). Tasks B1-B10 are in
# the cedian docs repo, docs/plans/done/s2-workflow-core.md.
#
#   script/cedian-bench/run.sh check <id|all>   prove each done predicate: it passes at the
#                                        reference commit and fails at the start commit.
#                                        No model, no OMP.
#   BENCH_GO=1 script/cedian-bench/run.sh run <id|all>
#                                        run cedian (this checkout's release build) on the
#                                        task in a fresh worktree, then check the predicate.
#                                        Needs the owner's go (ADR-0037).
#
# Work dir: $BENCH_WORK (default /tmp/cedian-bench). Results: $BENCH_WORK/results/<id>.json.
set -euo pipefail

ROOT=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
BENCH=$ROOT/script/cedian-bench
# The tasks start from commits in the cedian docs repo, which keeps the code
# history from before the move into the fork (ADR-0042). The binary under test
# is built here.
TASKS=${BENCH_TASKS_REPO:-$HOME/cedian}
WORK=${BENCH_WORK:-/tmp/cedian-bench}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$WORK/target}
export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"
# cedian spawns the first omp on PATH; pin it so the recorded version is the one that ran.
export CEDIAN_OMP_BINARY=${CEDIAN_OMP_BINARY:-$(command -v omp)}
ALL="b1 b2 b3 b4 b5 b6 b7 b8 b9 b10"
# B8 is open work: it starts where the harness was written.
B8_BASE=ef2f8b7

task() {
  case $1 in
  b1) BASE=e98674d^ REF=e98674d
      PROMPT="In crates/cedian_workspace, HostTools resolve() rejects buffer keys that start with a slash, like /notes.txt, even when the key names an open buffer or a file inside the workspace; OMP 18.6 sends keys in that form. Make resolve accept a /-key when it names an open buffer or a workspace file, and keep rejecting real absolute paths outside the workspace. Add tests." ;;
  b2) BASE=e98674d^ REF=e98674d
      PROMPT="OMP 18.6 runs cedian host tools as writes to xd://<tool> (for example a write whose path is xd://cedian_apply_edit). In crates/cedian_agent_ui the tool cards show these as a file write. Name such a card after the host tool instead (tool_card::card_for_tool and the thread's tool cards in message.rs); a read of xd://<tool> (OMP reading the tool's docs) stays a read and is never shown as an edit. Put the detection in pub fn tool_card::host_device<'a>(name: &str, preview: &'a str) -> Option<(&'a str, bool)>: Some((tool, true)) for a write to xd://<tool>, Some((tool, false)) for a read of it, None otherwise (other tools, other paths, an empty device). In render_thread, the write becomes a card named after the tool, titled 'Host tool' with the tool name as its preview; the read keeps the name read and is titled 'Read host tool docs'. Add tests." ;;
  b3) BASE=6b0aac0^ REF=6b0aac0
      PROMPT="cedian_worker::worktree::remove deletes a worker's branch with git branch -D, which destroys commits that were never merged. Make remove return an error and keep the branch (and its commits) when the worker branch has unmerged commits; a worker with nothing unmerged is still removed. Add tests." ;;
  b4) BASE=6b0aac0^ REF=6b0aac0
      PROMPT="HostTools::attach_lsp in crates/cedian_workspace deadlocks when a buffer is open: it holds the store lock while syncing buffers that need the same lock. Fix it so cargo test -p cedian_workspace finishes. The fake LSP bridge test (crates/cedian_workspace/tests/fake_lsp_bridge.rs) hardcodes one machine's paths; make it use paths relative to the crate manifest." ;;
  b5) BASE=264680f^ REF=264680f
      PROMPT="Give cedian_omp's EventRouter a log of finished tool calls. A call enters it only when its ToolEnd arrives (a ToolStart alone never does). Add EventRouter::finished_tool_call(tool_call_id) -> Option<FinishedToolCall>, where FinishedToolCall { tool_name: String, args_preview: String, is_error: bool } (args_preview from the ToolStart) derives Debug, Clone, PartialEq, Eq and is exported from the crate root. Add tests." ;;
  b6) BASE=378d623^ REF=378d623
      PROMPT="Headless cedian (the CLI) never answers OMP's extension UI dialogs, so an approval dialog stalls the turn until the prompt timeout. Add a module cedian_omp::headless_ui with pub fn headless_answer(&ExtensionUiRequest) -> Option<(ExtensionUiResponse, String)> that fails closed: a select gets the option named deny (case-insensitive) or is cancelled when there is none; a confirm is declined; input, editor and ask are cancelled; fire-and-forget requests (notify, status, widget and the like) get None. The String is a one-line label of the dialog title. Wire it into OmpRuntime (deny_ui_requests() turns it on; take_refused_ui_requests() returns the labels) and have the CLI print '[✗] refused (no UI to approve): <label>' for each after a turn. The replay fixture crates/cedian_cli/tests/fixtures/p5_worktree.jsonl is provided." ;;
  b7) BASE=09e88b6^ REF=09e88b6
      PROMPT="The worker registry (.cedian/workers.json, cedian_worker::Registry) has no snapshot_version, and a corrupt file is silently treated as empty. Add snapshot_version 1. Registry::open(repo) changes from returning (Registry, bool) to Result<(Registry, bool), WorkerError> and fails with a new WorkerError::Snapshot(String) when the version is missing or different, or the file is not valid JSON. Add tests." ;;
  b8) BASE=$B8_BASE REF=
      PROMPT="cedian workflow run in crates/cedian_cli/src/main.rs hand-parses the task kind and risk with string match arms. Reuse the serde names the workflow channel already uses for TaskKind and Risk instead, so there is one spelling. cedian workflow run with an unknown kind must still fail and list every kind." ;;
  b9) BASE=3d66849^ REF=3d66849
      PROMPT="Write crates/cedian_omp/tests/omp_parity.rs: a test that parses the vendored vendor/omp-rpc/src/wire.rs for every RPC command, server notification and extension UI request (by wire name) and fails, naming each feature, when docs/OMP_PARITY.md has no row mentioning it in backticks. It must also fail when the parser finds fewer than 60 commands, 40 notifications or 10 UI requests (the parser no longer matching the generated file). Add any missing ledger rows so it passes." ;;
  b10) BASE=09e88b6^ REF=09e88b6
      PROMPT="The browser store (.cedian/browser.json, crates/cedian_cli/src/browser_store.rs) has no snapshot_version. Add one; loading a file whose version is missing or different must fail with an error that says the state is too old and names the reset command, cedian browser open <url>. Add tests." ;;
  *) echo "unknown task $1" >&2; exit 2 ;;
  esac
}

splice() { python3 "$BENCH/splice_tests.py" "$1" "$REF"; }

# The done predicate, run inside a checkout of the result. Exit 0 = done.
predicate() {
  local id=$1
  case $id in
  b1) splice crates/cedian_workspace/src/host.rs &&
      cargo test -q -p cedian_workspace --lib host:: ;;
  b2) splice crates/cedian_agent_ui/src/message.rs &&
      splice crates/cedian_agent_ui/src/tool_card.rs &&
      cargo test -q -p cedian_agent_ui --lib ;;
  b3) mkdir -p crates/cedian_worker/tests &&
      cp "$BENCH/hidden/b3_remove.rs" crates/cedian_worker/tests/bench_b3.rs &&
      cargo test -q -p cedian_worker --test bench_b3 ;;
  b4) git show "$REF:crates/cedian_workspace/tests/fake_lsp_bridge.rs" \
        > crates/cedian_workspace/tests/fake_lsp_bridge.rs &&
      cargo test -q -p cedian_workspace --no-run &&
      perl -e 'alarm shift; exec @ARGV' 60 cargo test -q -p cedian_workspace ;;
  b5) splice crates/cedian_omp/src/event_router.rs &&
      cargo test -q -p cedian_omp --lib event_router ;;
  b6) splice crates/cedian_omp/src/headless_ui.rs &&
      git show "$REF:crates/cedian_cli/tests/replay_cli.rs" > crates/cedian_cli/tests/replay_cli.rs &&
      cargo test -q -p cedian_omp --lib headless_ui &&
      cargo test -q -p cedian_cli --test replay_cli ;;
  b7) splice crates/cedian_worker/src/registry.rs &&
      cargo test -q -p cedian_worker --lib registry ;;
  b8) ! grep -q '"bug_fix" =>' crates/cedian_cli/src/main.rs &&
      cargo build -q -p cedian_cli &&
      b8_unknown_kind_lists_kinds ;;
  b9) b9_parity_test ;;
  b10) splice crates/cedian_cli/src/browser_store.rs &&
       python3 "$BENCH/insert_test.py" crates/cedian_cli/src/browser_store.rs "$BENCH/hidden/b10_reset.rs" &&
       cargo test -q -p cedian_cli --bin cedian browser_store ;;
  esac && cargo test -q --workspace
}

# The predicate's outcome, split so a correct fix whose private names differ
# from the reference commit is not scored as a failure. Prints one word:
#   pass                 the predicate holds
#   fail                 the result does not build, or builds and the predicate fails
#   hidden_tests_broken  the result builds but the hidden tests do not compile
#                        against it; neither pass nor false-done, the owner reviews it
verdict() {
  local id=$1
  cargo build -q --workspace --all-targets >&2 || { echo fail; return; }
  predicate "$id" >&2 && { echo pass; return; }
  if cargo test -q --workspace --no-run >&2; then echo fail; else echo hidden_tests_broken; fi
}

b8_unknown_kind_lists_kinds() {
  local tmp out
  tmp=$(mktemp -d)
  printf 'schema = 1\n' > "$tmp/cedian.toml"
  if out=$(CEDIAN_CONFIG=$tmp/cedian.toml CEDIAN_WORKDIR=$tmp \
      "$CARGO_TARGET_DIR/debug/cedian" workflow run nope x 2>&1); then
    echo "b8: unknown kind accepted" >&2; return 1
  fi
  for kind in investigation bug_fix feature refactor performance prototype; do
    grep -q "$kind" <<<"$out" || { echo "b8: error does not list $kind: $out" >&2; return 1; }
  done
}

b9_parity_test() {
  local t=crates/cedian_omp/tests/omp_parity.rs out
  test -f $t || { echo "b9: no $t" >&2; return 1; }
  for floor in 60 40 10; do
    grep -q "\b$floor\b" $t || { echo "b9: no floor $floor in $t" >&2; return 1; }
  done
  cargo test -q -p cedian_omp --test omp_parity || return 1
  cp docs/OMP_PARITY.md "$WORK/parity.bak"
  sed -i '' 's/^| `steer` |/| `steer_renamed` |/' docs/OMP_PARITY.md
  if out=$(cargo test -q -p cedian_omp --test omp_parity 2>&1); then
    cp "$WORK/parity.bak" docs/OMP_PARITY.md
    echo "b9: test still passes with the steer row renamed" >&2; return 1
  fi
  cp "$WORK/parity.bak" docs/OMP_PARITY.md
  grep -q 'steer' <<<"$out" || { echo "b9: failure does not name steer" >&2; return 1; }
}

worktree() { # <name> <rev> -> path
  local dir=$WORK/trees/$1
  git -C "$TASKS" worktree remove --force "$dir" 2>/dev/null || true
  git -C "$TASKS" worktree add -q --detach "$dir" "$2"
  echo "$dir"
}

drop() { git -C "$TASKS" worktree remove --force "$1"; }

check() {
  local id=$1 dir v
  task "$id"
  if [ -n "$REF" ]; then
    dir=$(worktree "$id-ref" "$REF")
    if (cd "$dir" && predicate "$id") >"$WORK/logs/$id-ref.log" 2>&1; then
      echo "$id: passes at $REF"
    else
      echo "$id: FAILS at its reference commit $REF (predicate wrong; see $WORK/logs/$id-ref.log)"
      drop "$dir"; return 1
    fi
    drop "$dir"
  fi
  dir=$(worktree "$id-base" "$BASE")
  v=$(cd "$dir" && verdict "$id" 2>"$WORK/logs/$id-base.log")
  if [ "$v" = pass ]; then
    echo "$id: PASSES at its start commit $BASE (predicate does not detect the task)"
    drop "$dir"; return 1
  fi
  echo "$id: $v at $BASE"
  drop "$dir"
}

run() {
  local id=$1 dir cfg start end out verdict
  [ "${BENCH_GO:-}" = 1 ] || { echo "run needs the owner's go: BENCH_GO=1 (ADR-0037)" >&2; exit 2; }
  task "$id"
  dir=$(worktree "$id-run" "$BASE")
  [ "$id" = b6 ] && git show "$REF:crates/cedian_cli/tests/fixtures/p5_worktree.jsonl" \
      > "$dir/crates/cedian_cli/tests/fixtures/p5_worktree.jsonl"
  dir=$(cd "$dir" && pwd -P)
  cfg=$WORK/run-$id.toml
  # The agent must run cargo: the run uses the owner's OMP config (ADR-0035).
  printf 'schema = 1\n[projects."%s"]\npolicy = "omp"\n' "$dir" > "$cfg"
  rm -f "$WORK/results/$id.timing.jsonl"
  start=$(python3 -c 'import time; print(int(time.time()*1000))')
  out=$(CEDIAN_CONFIG=$cfg CEDIAN_WORKDIR=$dir CEDIAN_SESSION_DIR=$WORK/sessions/$id \
        CEDIAN_TIMING=$WORK/results/$id.timing.jsonl \
        "$CARGO_TARGET_DIR/release/cedian" prompt "$PROMPT" 2>&1) || true
  end=$(python3 -c 'import time; print(int(time.time()*1000))')
  printf '%s\n' "$out" > "$WORK/logs/$id-run.out"
  verdict=$(cd "$dir" && verdict "$id" 2>"$WORK/logs/$id-predicate.log")
  python3 - "$id" "$verdict" "$((end - start))" "$WORK" "$dir" <<'PY'
import json, os, re, subprocess, sys
id, verdict, wall, work = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4]
out = open(f"{work}/logs/{id}-run.out").read()
timing = [json.loads(l) for l in open(f"{work}/results/{id}.timing.jsonl")] if __import__("os").path.exists(f"{work}/results/{id}.timing.jsonl") else []
claimed = bool(re.search(r"\b(done|fixed|complete[d]?|implemented)\b", out.split("\n[")[0], re.I))
omp_bin = os.environ["CEDIAN_OMP_BINARY"]
omp = subprocess.run([omp_bin, "--version"], capture_output=True, text=True).stdout.strip()
model = json.loads(subprocess.run([omp_bin, "config", "get", "modelRoles", "--json"], capture_output=True, text=True, cwd=sys.argv[5]).stdout)["value"].get("default")
json.dump({
    "task": id, "predicate": verdict, "wall_ms": wall,
    "time_to_usable_result_ms": wall if verdict == "pass" else None,
    "claimed_done": claimed, "false_done": claimed and verdict == "fail",
    "omp_version": omp, "omp_binary": omp_bin, "model": model, "timing": timing,
}, open(f"{work}/results/{id}.json", "w"), indent=2)
print(f"{id}: predicate {verdict}, wall {wall} ms, claimed_done {claimed}")
PY
}

mode=${1:-}; target=${2:-}
[ -n "$mode" ] && [ -n "$target" ] || { sed -n '2,13p' "$0"; exit 2; }
mkdir -p "$WORK/logs" "$WORK/results" "$WORK/trees"
[ "$target" = all ] && ids=$ALL || ids=$target
[ "$mode" = run ] && (cd "$ROOT" && cargo build -q --release -p cedian_cli) &&
  { [ -x "$CARGO_TARGET_DIR/release/cedian" ] || { echo "no cedian binary at $CARGO_TARGET_DIR/release" >&2; exit 2; }; }
status=0
for id in $ids; do "$mode" "$id" || status=1; done
exit $status
