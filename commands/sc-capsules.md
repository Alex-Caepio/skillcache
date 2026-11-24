---
description: List, inspect, requalify, invalidate, or retire SkillCache capsules for a repository
argument-hint: "[--project <repo-root>] [--skill <name>] [--status active|stale|retired]"
allowed-tools: ["Bash", "Read"]
---

# SkillCache Capsules

User arguments: `$ARGUMENTS`

Follow the `skillcache:skillcache-capsules` skill:

1. Determine the explicit repository root `<repo-root>` and requested action (`list`, `inspect <id>`, `requalify <id>`, `invalidate <id>`, or `retire <id>`).
2. Call the corresponding MCP tool (`skillcache_capsules_list`, `skillcache_capsule_inspect`, `skillcache_requalify`, `skillcache_invalidate`, or `skillcache_retire`) passing `"project": "<repo-root>"` explicitly.
3. Summarize capsule IDs, forms (`hybrid`/`textual`), statuses, stale reasons, and gate history cleanly.
