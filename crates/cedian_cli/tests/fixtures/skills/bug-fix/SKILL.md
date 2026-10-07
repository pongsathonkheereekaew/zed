---
name: bug-fix
description: Fix a bug under a cedian workflow — reproduce, investigate, implement, verify — reporting each step to the cedian IDE so it can check the fix before you call it done. Use when asked to fix a bug in a workspace hosted by cedian.
---

# Bug fix (cedian playbook)

cedian is the IDE hosting this workspace. `cedian_workflow_update` and `cedian_complete` are its own trusted host tools. cedian does not take your word that the bug is fixed. It checks evidence bound to tool calls you actually ran, and it refuses completion until the required gates pass.

## 1. Start

```json
{"op": "start", "kind": "bug_fix", "title": "<one line>", "risk": "low"}
```

Use `risk` `medium` or `high` when the fix touches shared code or user data.

## 2. Reproduce

Run something that shows the bug: a failing test, a command, or a `read` of the wrong output. Then report what that call showed. `from_tool` is the tool you used, and `match` is a piece of its arguments:

```json
{"op": "evidence", "gate": "reproduce", "kind": "command", "outcome": "fail", "summary": "<what you saw>", "from_tool": "read", "match": "<file or command>"}
```

`outcome` `fail` means the bug showed. Then advance:

```json
{"op": "advance", "passed": true}
```

## 3. Investigate, then implement

Find the cause. Advance. Make the smallest fix. Advance again.

## 4. Verify

After the fix, run the check again. Evidence from before the fix doesn't count: cedian marks it stale once a file it saw changes.

```json
{"op": "evidence", "gate": "verify", "kind": "test", "outcome": "pass", "summary": "<what passed>", "from_tool": "bash", "match": "<command>"}
```

If you could not run the check (a tool was refused or unavailable), report `outcome` `inconclusive`. Never `pass`. Then advance.

## 5. Complete

Call `cedian_complete` with a claim for everything you say is true:

```json
{"summary": "<one line>", "claims": [{"text": "<claim>", "label": "measured", "evidence": ["e2"]}]}
```

- `measured`: you ran it, and the evidence id cedian returned shows it.
- `inferred`: it follows from evidence, and you cite the ids.
- `guess`: you did not check it.

If `cedian_complete` refuses, it lists the missing gates. Produce that evidence and call it again. If you cannot, stop and tell the user which gates are missing. Do not say the bug is fixed.
