---
name: skillcache-warm-run
description: Use this skill when executing or testing a sealed hybrid SkillCache capsule directly via `skillcache-engine run|continue|status|reset|tool` or multicall `skillcache-run` and `skillcache-tool` entrypoints inside a warm session.
version: 0.1.0
license: Apache-2.0
---

# SkillCache Warm Execution & Multicall Binaries

Hybrid capsules execute deterministic bounded steps (`program` and `ask` / `agent` handoffs) through the SkillCache executor and sandboxed program runner.

## Multicall Entrypoints (`argv[0]` Dispatch)

The `skillcache-engine` binary acts as both the standalone CLI/MCP binary and a multicall executable when symlinked or invoked via `argv[0]`:

- **`skillcache-run`** -> dispatches directly to the warm plan executor (`run`, `continue`, `status`, `reset`)
- **`skillcache-tool`** -> dispatches directly to single-program execution (`<program> --input '<JSON>' | --check`)

Direct subcommands on `skillcache-engine` also expose the same operations:
- `skillcache-engine run ['<JSON>'] [--replay <examples.json>]`
- `skillcache-engine continue ['<JSON>']`
- `skillcache-engine status` (note: runs executor status when `SKILLCACHE_PACKAGE` + `SKILLCACHE_STATE_FILE` are set and `--project` is absent; otherwise inspects project status)
- `skillcache-engine reset`
- `skillcache-engine tool <program> --input '<JSON>' | --check`

## Required Staged Environment Variables

| Variable | Purpose |
| --- | --- |
| `SKILLCACHE_PACKAGE` | Sealed warm package directory containing `capsule.json` |
| `SKILLCACHE_WORKSPACE` | Target workspace directory (defaults to current working directory) |
| `SKILLCACHE_STATE_FILE` | Writable path for step cursor & memoization JSON |
| `SKILLCACHE_GENERATION` | Optional run correlation identifier |
| `SKILLCACHE_LOG_FILE` | Optional JSONL journal log path |

## Execution Workflow

1. Run `skillcache-run run '{"binding_key": "value"}'` (or `-` on stdin for JSON).
2. Read the single JSON object printed to stdout:
   - `"status": "complete"` -> inspect `"result"` or final output steps.
   - `"status": "needs_input"` -> inspect `"prompt"` / `"missing"` fields and supply them via `skillcache-run continue '{"missing_key": ...}'`.
   - `"status": "refused"` -> inspect `"reason"` and `"message"`.
3. For isolated single-tool checks: run `skillcache-tool <program_id> --check` or `--input '{"arg": "val"}'`.
