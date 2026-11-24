# skillcache

Standalone dual-compatible plugin for **Claude Code (`2.1.278+`)** and **Codex CLI (`0.169.0+`)** providing the extracted SkillCache capsule compiler, sandboxed Gate 1/2/3 verification engine, warm capsule executor (`skillcache-run` / `skillcache-tool`), private SQLite capsule/trace registry, and stdio MCP server (`2025-06-18`).

## Highlights

- **Standalone Binary (`bin/skillcache-engine`)**: Native Rust binary with zero monorepo runtime dependencies.
- **Multicall `argv[0]` Dispatch**:
  - `skillcache-run` -> warm plan executor (`run`, `continue`, `status`, `reset`)
  - `skillcache-tool` -> single-program runner (`<program> --input '<JSON>' | --check`)
- **Three-Gate Qualification Pipeline**:
  - **Gate 1** (`skillcache_gate1` / `skillcache-engine gate1`): Static schema, allowlist, secret-safety, and binding dataflow validation.
  - **Gate 2** (Sandboxed Program Check): Executes every program's `--check` inside an isolated `--unshare-net` Linux `bwrap` sandbox (or `sandbox-exec` on macOS). Fails closed if sandboxing is unavailable.
  - **Gate 3** (Sandboxed Replay Verification): Replays the plan against the captured starting snapshot and expected observed outputs inside the sandbox.
- **Private Local State**: Stored outside your repository under `$SKILLCACHE_STATE_DIR` (or `$XDG_STATE_HOME/skillcache`, defaulting to `~/.local/state/skillcache`) with `0o700` directory and `0o600` database permissions (`skillcache.sqlite3`).
- **`plugin-cwd` Guard**: Refuses omitted, `""`, or `"."` project paths whenever the working directory lies inside the packaged plugin or marketplace cache directory. Always pass an explicit `project` path from plugin-scoped MCP calls.

## MCP Tools (JSON-RPC 2.0 / MCP `2025-06-18`)

Exposed via `skillcache-engine mcp` (`.mcp.json` for Claude Code, `.codex-mcp.json` for Codex):

| Tool | Read-Only | Purpose |
| --- | --- | --- |
| `skillcache_status` | Yes | Inspect sandbox readiness, compiler CLIs (`claude`/`codex`), traces, capsule counts & amortization |
| `skillcache_capsules_list` | Yes | List compiled capsules with status, form, stale reasons & amortization |
| `skillcache_capsule_inspect` | Yes | Inspect capsule details, gate reports, dependency manifest & runs |
| `skillcache_gate1` | Yes | Validate a package directory with Gate 1 static/dataflow checks |
| `skillcache_compile` | No | Compile from a trace generation, trace JSON, candidate JSON/dir, or skill via Gate 1/2/3 |
| `skillcache_requalify` | No | Re-verify an existing capsule against current skill/workspace without invoking an LLM |
| `skillcache_retire` | No | Retire a capsule with an optional reason |
| `skillcache_invalidate` | No | Mark an active/needs_recompile capsule as stale |
| `skillcache_traces` | No | List, inspect, ingest, or delete cold-turn observation traces |

## CLI Usage

```sh
# Status & capsule registry
bin/skillcache-engine status --project /path/to/repo
bin/skillcache-engine capsules --project /path/to/repo
bin/skillcache-engine inspect <capsule_id> --project /path/to/repo

# Gate 1 validation on a package folder
bin/skillcache-engine gate1 /path/to/package [--examples /path/to/examples.json]

# Ingest trace & compile
bin/skillcache-engine traces --project /path/to/repo --action ingest --trace-path trace.json
bin/skillcache-engine compile --project /path/to/repo --generation <gen-id> --provider claude
bin/skillcache-engine compile --project /path/to/repo --candidate candidate.json

# Requalify / invalidate / retire
bin/skillcache-engine requalify <capsule_id> --project /path/to/repo
bin/skillcache-engine invalidate <capsule_id> --project /path/to/repo
bin/skillcache-engine retire <capsule_id> --project /path/to/repo --reason "Replaced"
```

## Slash Commands & Skills

- Commands: `/sc-status`, `/sc-compile`, `/sc-capsules`, `/sc-gate1`
- Skills: `skillcache-compile`, `skillcache-capsules`, `skillcache-warm-run`

## Build & Test

```sh
CARGO_TARGET_DIR=/path/to/build/skillcache CARGO_INCREMENTAL=0 cargo test --offline --locked
CARGO_TARGET_DIR=/path/to/build/skillcache CARGO_INCREMENTAL=0 cargo build --release --offline --locked --bin skillcache-engine
```
