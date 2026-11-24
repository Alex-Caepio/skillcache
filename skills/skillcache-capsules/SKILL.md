---
name: skillcache-capsules
description: Use this skill when listing compiled SkillCache capsules, inspecting gate reports and dependency manifests, requalifying stale capsules through Gate 1/2/3 without re-running an LLM, or invalidating/retiring outdated capsules.
version: 0.1.0
license: Apache-2.0
---

# SkillCache Capsule Lifecycle Management

Manage compiled SkillCache capsules stored under `$SKILLCACHE_STATE_DIR` (or `$XDG_STATE_HOME/skillcache` / `~/.local/state/skillcache`) so target repositories never carry untracked SQLite files.

## Important: Explicit Project Path

> Always pass `"project": "<repository-root>"` explicitly to `skillcache_*` MCP tools. The `plugin-cwd` guard rejects omitted/`"."` targets when the server runs inside the packaged plugin directory.

## Operations

1. **List capsules & check staleness/amortization**:
   - Call `skillcache_capsules_list` with `{"project": "<repo-root>"}` (optional filters: `"skill"`, `"status"`, `"form"`, `"include_retired": true`).
   - CLI equivalent:
     ```sh
     skillcache-engine capsules --project "<repo-root>"
     ```
2. **Inspect a single capsule's gate verdicts, capabilities, and dependency manifest**:
   - Call `skillcache_capsule_inspect` with `{"project": "<repo-root>", "capsule_id": "<id>"}`.
   - CLI equivalent:
     ```sh
     skillcache-engine inspect "<capsule-id>" --project "<repo-root>"
     ```
3. **Invalidate an active capsule manually**:
   - Marks `active` / `needs_recompile` capsules `stale` so warm sessions fall back to cold execution until requalified:
     `skillcache_invalidate` with `{"project": "<repo-root>", "capsule_id": "<id>"}`.
4. **Requalify a stale, needs_recompile, or retired capsule**:
   - Re-runs Gate 1, Gate 2, and Gate 3 against current disk state without invoking an LLM (requires the underlying `SKILL.md` tree SHA to match):
     `skillcache_requalify` with `{"project": "<repo-root>", "capsule_id": "<id>"}`.
5. **Retire a superseded capsule**:
   - Call `skillcache_retire` with `{"project": "<repo-root>", "capsule_id": "<id>", "reason": "<why>"}`.
