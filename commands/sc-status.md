---
description: Inspect SkillCache sandbox readiness, compiler CLIs, trace count, capsule status counts, and amortization for a repository
argument-hint: "[--project <repo-root>]"
allowed-tools: ["Bash", "Read"]
---

# SkillCache Status

User arguments: `$ARGUMENTS`

1. Determine the explicit repository root `<repo-root>` (from `--project` in `$ARGUMENTS` or current working directory if it is not inside the plugin directory).
2. Call MCP tool `skillcache_status` with `{"project": "<repo-root>"}` (always pass `"project"` explicitly so Codex `cwd: "."` does not fail the `plugin-cwd` guard).
3. If MCP is unavailable, run:
   ```sh
   skillcache-engine status --project "<repo-root>"
   ```
4. Summarize sandbox availability, available compiler CLIs (`claude`, `codex`), capsule status counts, stale reasons, trace count, and estimated break-even reuse amortization.
