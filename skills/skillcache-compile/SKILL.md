---
name: skillcache-compile
description: Use this skill when compiling an observed agent turn trace or candidate package into a verified SkillCache capsule, validating a capsule package with Gate 1, Gate 2 (--unshare-net sandbox), or Gate 3 replay, or importing a trace JSON file.
version: 0.1.0
license: Apache-2.0
---

# SkillCache Compilation & Gate Verification

Use this skill to turn repeatable agent workflows (`SKILL.md` turns or pre-authored candidate packages) into sealed, gate-verified hybrid or textual capsules stored in private local SQLite state.

## Important: Codex & Plugin-Scoped MCP Working Directory

> Always pass `"project": "<repository-root>"` explicitly when calling any `skillcache_*` MCP tool (`skillcache_status`, `skillcache_compile`, `skillcache_traces`, etc.). Because `.codex-mcp.json` sets `"cwd": "."`, omitting `"project"` or passing `"."` while `cwd` is inside the cached plugin directory is refused by the `plugin-cwd` guard.
> Pass an explicit `"package_dir"` to `skillcache_gate1`.

## Gate Model

1. **Gate 1 (`skillcache_gate1` / `skillcache-engine gate1`)**: Static structural validation of `capsule.json`, schema version, compile rules version, binding dataflow, runtime allowlists (`python3`, `php`, `node`, `bash`, `sh`), network declarations, and secret-redaction checks.
2. **Gate 2 (Sandboxed Program Check)**: Executes every hybrid program's `--check` contract in a minimal isolated sandbox (`bubblewrap --unshare-net` on Linux or `sandbox-exec` on macOS) over an empty folder — fails closed without confinement.
3. **Gate 3 (Sandboxed Replay Verification)**: Replays the capsule plan against the captured starting snapshot and observed trace values, checking structural equality of workspace diffs and output text/JSON.

## Procedure

1. **Verify sandbox & compiler availability**:
   - Call `skillcache_status` with `{"project": "<repo-root>"}`.
   - CLI equivalent:
     ```sh
     skillcache-engine status --project "<repo-root>"
     ```
2. **Validate a standalone capsule package with Gate 1 (optional pre-flight)**:
   - Call `skillcache_gate1` with `{"package_dir": "<abs-package-dir>", "examples_path": "<optional-examples.json>"}`.
   - CLI equivalent:
     ```sh
     skillcache-engine gate1 "<package-dir>" --examples "<examples.json>"
     ```
3. **Ingest or inspect recorded cold-turn traces**:
   - Call `skillcache_traces` with `{"project": "<repo-root>", "action": "list"}` or `{"project": "<repo-root>", "action": "ingest", "trace_path": "<trace.json>"}`.
4. **Compile into a verified capsule**:
   - **From a trace generation**:
     Call `skillcache_compile` with `{"project": "<repo-root>", "generation": "<gen-id>", "provider": "claude"}` (`"codex"` also supported).
   - **From a trace file directly**:
     Call `skillcache_compile` with `{"project": "<repo-root>", "trace_path": "<trace.json>", "provider": "claude"}`.
   - **From a pre-authored candidate JSON or directory (direct Gate 1/2/3 qualification, no LLM roundtrip)**:
     Call `skillcache_compile` with `{"project": "<repo-root>", "candidate_path": "<candidate-json-or-dir>"}`.
   - CLI equivalent:
     ```sh
     skillcache-engine compile --project "<repo-root>" --candidate "<candidate.json>"
     ```
