---
description: Run static Gate 1 structural, allowlist, dataflow, and secret-safety checks on a SkillCache package directory
argument-hint: "<package-dir> [--examples <examples.json>]"
allowed-tools: ["Bash", "Read"]
---

# SkillCache Gate 1

User arguments: `$ARGUMENTS`

1. Extract `<package-dir>` and optional `--examples <file>` from `$ARGUMENTS` (must point outside the plugin directory).
2. Call MCP tool `skillcache_gate1` with `{"package_dir": "<package-dir>"}` (and optional `"examples_path"`).
3. If MCP is unavailable, run:
   ```sh
   skillcache-engine gate1 "<package-dir>" --examples "<examples.json>"
   ```
4. Report whether Gate 1 passed and list every violation code, field, and remediation message if any check failed.
