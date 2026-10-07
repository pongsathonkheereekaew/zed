---
name: verify-notes
description: Verification profile for the notes app in this workspace — launch an instance, check it with Doctor, drive a feature, collect evidence, clean up — and report every stage to the cedian IDE. Use when asked to verify or prove a notes-app feature.
---

# verify-notes (verification profile)

cedian is the IDE hosting this workspace. It does not run this profile and does not take your word for it. It records each stage you report through its host tool `cedian_workflow_update`, bound to the `bash` call that ran it. A feature counts as proven only with evidence from a profile that has run end to end, from an instance whose Doctor passed since its last surprising drive.

Every stage is one `bash` call. Run it from the workspace root, then report it right away. `instance` names the running app instance you launched (`i1`, `i2`, …). `match` is the script name, so cedian binds the report to that call.

## Launch

Start a fresh instance:

```sh
sh launch.sh <instance>
```

```json
{"op": "profile", "profile": "verify-notes", "stage": "launch", "instance": "<instance>", "ok": true, "from_tool": "bash", "match": "launch.sh"}
```

## Doctor

Check the instance answers before you trust it:

```sh
sh doctor.sh <instance>
```

```json
{"op": "profile", "profile": "verify-notes", "stage": "doctor", "instance": "<instance>", "ok": true, "from_tool": "bash", "match": "doctor.sh"}
```

`ok` is `false` when the script printed no `doctor … ok` line.

## Drive

Exercise one feature from the feature map:

```sh
sh drive.sh add-note
```

```json
{"op": "profile", "profile": "verify-notes", "stage": "drive", "instance": "<instance>", "ok": true, "surprising": false, "from_tool": "bash", "match": "drive.sh"}
```

Set `surprising` to `true` when the drive did something you did not expect. The instance then needs a passing Doctor before its evidence counts again.

## Evidence

Collect what the feature left behind:

```sh
sh evidence.sh add-note
```

```json
{"op": "profile", "profile": "verify-notes", "stage": "evidence", "instance": "<instance>", "ok": true, "from_tool": "bash", "match": "evidence.sh"}
```

To use that observation as feature evidence for a gate, report it too:

```json
{"op": "evidence", "gate": "add-note", "kind": "command", "outcome": "pass", "summary": "<what evidence.sh printed>", "from_tool": "bash", "match": "evidence.sh", "profile": "verify-notes", "feature": "add-note", "instance": "<instance>"}
```

## Cleanup

```sh
sh cleanup.sh <instance>
```

```json
{"op": "profile", "profile": "verify-notes", "stage": "cleanup", "instance": "<instance>", "ok": true, "from_tool": "bash", "match": "cleanup.sh"}
```

## Feature map

- `add-note`: adding a note makes it show up in the list
