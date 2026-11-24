//! Recording the observed trace of a cold turn.
//!
//! The run's one event slot belongs to the lifecycle binding; a [`Tee`] gives
//! the recorder the same raw provider events without changing what the
//! binding sees. Observing is cheap and never fails the run. When the turn
//! completes, the trace is redacted, bounded and stored; a turn that failed,
//! used no tool or no identifiable skill leaves nothing behind.
use crate::engine::db;
use crate::engine::state_store::{ResolvedCommand, RunBinding, SessionStore};
use crate::error::Result;
use crate::host::compiler_launch as run_activity;
use crate::host::{process::RunObserver, provider::RunEvents};
use crate::core::{
    env_filter, executor, fingerprint,
    trace::{Recorder, Redactor, SkillRef, Trace},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// The shortest environment value worth replacing wherever it appears.
const SECRET_CHARS: usize = 8;
const SKILLS: usize = 8;
pub const SKILL_FOLDERS: [&str; 3] = [".claude/skills", ".agents/skills", ".cursor/skills"];

/// Whether `workspace` or the user's home directory contains at least one
/// discoverable `SKILL.md` folder for Claude, Codex, or Cursor.
pub fn workspace_has_skills(workspace: &Path) -> bool {
    let home = run_activity::user_home().map(PathBuf::from);
    for root in [Some(workspace), home.as_deref()].into_iter().flatten() {
        for folder in SKILL_FOLDERS {
            let Ok(entries) = std::fs::read_dir(root.join(folder)) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path().join("SKILL.md");
                if std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file()) {
                    return true;
                }
            }
        }
    }
    false
}

/// Whether this cold turn should pay the pre-turn workspace snapshot + hash
/// cost before the provider runs.
///
/// Unrouted chat turns in `auto` mode carry no requested skill and skip
/// pre-turn hashing; explicit `observe` turns snapshot whenever a skill is
/// routed or at least one skill folder is present in the workspace/home.
pub fn should_snapshot_for_observation(
    session: &Value,
    route: &ResolvedCommand,
    workspace: &Path,
) -> bool {
    if !route.requests.is_empty() || route.mode == "native" {
        return true;
    }
    crate::engine::settings::mode(session) == "observe" && workspace_has_skills(workspace)
}

/// As [`should_snapshot_for_observation`], also enabling observation tracking
/// when opt-in `ToolChain` capture is enabled on `store` or `session`.
pub fn should_snapshot_for_observation_in_store(
    store: &SessionStore,
    session: &Value,
    route: &ResolvedCommand,
    workspace: &Path,
) -> bool {
    should_snapshot_for_observation(session, route, workspace)
        || (crate::engine::settings::records(session)
            && crate::engine::settings::tool_chain_enabled(store, session))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObservationStrategy {
    None,
    Full,
    LightweightToolchain,
}

/// Chooses pre-turn observation work: routed/explicit skill observations take a
/// full pre-turn snapshot; unrouted `ToolChain` candidates use a lightweight
/// pre/post git/stat fingerprint so cold turns avoid heavy `ignored/` copies.
pub(crate) fn observation_strategy(
    store: &SessionStore,
    session: &Value,
    route: &ResolvedCommand,
    workspace: &Path,
) -> ObservationStrategy {
    if should_snapshot_for_observation(session, route, workspace) {
        ObservationStrategy::Full
    } else if crate::engine::settings::records(session)
        && crate::engine::settings::tool_chain_enabled(store, session)
    {
        ObservationStrategy::LightweightToolchain
    } else {
        ObservationStrategy::None
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ToolchainPreTurnStamp {
    pub digest: String,
}

fn stat_stamp_entry(path: &Path) -> String {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return "missing".into();
    };
    if crate::custody::fs::linked(&meta) {
        let target = std::fs::read_link(path)
            .map(|t| t.to_string_lossy().into_owned())
            .unwrap_or_default();
        return format!("link:{target}");
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    #[cfg(unix)]
    let (ctime, ino) = {
        use std::os::unix::fs::MetadataExt;
        (
            i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec()),
            meta.ino(),
        )
    };
    #[cfg(not(unix))]
    let (ctime, ino) = (0i128, 0u64);
    format!(
        "{}:{}:{mtime}:{ctime}:{ino}",
        if meta.is_dir() { "d" } else { "f" },
        meta.len()
    )
}

fn stat_tree_bounded(
    root: &Path,
    subdir: Option<&str>,
    allowlist: &[String],
    max_entries: usize,
    out: &mut Vec<(String, String)>,
) -> Option<()> {
    let start = match subdir {
        Some(rel) => root.join(rel),
        None => root.to_path_buf(),
    };
    if !start.exists() {
        return Some(());
    }
    let mut stack = vec![start];
    while let Some(dir) = stack.pop() {
        let mut entries = std::fs::read_dir(&dir)
            .ok()?
            .collect::<std::io::Result<Vec<_>>>()
            .ok()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            if out.len() >= max_entries {
                return None;
            }
            let path = entry.path();
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            if subdir.is_none()
                && (executor::is_ignored_workspace_name_with(name_str, allowlist)
                    || (dir == root && matches!(name_str, ".harness-tmp" | "target")))
            {
                continue;
            }
            if subdir.is_some() && executor::ALWAYS_IGNORED_WORKSPACE_NAMES.contains(&name_str) {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let stamp = stat_stamp_entry(&path);
            let is_dir = stamp.starts_with("d:");
            out.push((rel, stamp));
            if is_dir {
                stack.push(path);
            }
        }
    }
    Some(())
}

/// Captures a lightweight git+stat fingerprint of `workspace` (zero file reads or
/// `ignored/` copies) so unrouted `ToolChain` cold turns can verify `changes.is_empty()`
/// post-turn before capturing a replay snapshot.
pub(crate) fn capture_toolchain_pre_turn_stamp(
    workspace: &Path,
    allowlist: &[String],
) -> Option<ToolchainPreTurnStamp> {
    const STAT_LIMIT: usize = 25_000;
    if let Ok(located) = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["rev-parse", "--show-toplevel", "--show-prefix"])
        .output()
        && located.status.success()
    {
        let text = String::from_utf8_lossy(&located.stdout);
        let mut lines = text.lines();
        if let Some(top_raw) = lines.next().map(str::trim).filter(|s| !s.is_empty()) {
            let top = PathBuf::from(top_raw);
            let prefix = lines
                .next()
                .unwrap_or("")
                .trim()
                .trim_end_matches('/')
                .to_owned();
            let tree_ref = if prefix.is_empty() {
                "HEAD^{tree}".to_owned()
            } else {
                format!("HEAD:{prefix}")
            };
            let tree_oid = std::process::Command::new("git")
                .arg("-C")
                .arg(&top)
                .args(["rev-parse", "--verify", "--quiet", &tree_ref])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "UNBORN".to_owned());
            if let Ok(status_out) = std::process::Command::new("git")
                .arg("-C")
                .arg(workspace)
                .args([
                    "-c",
                    "core.quotePath=false",
                    "status",
                    "--porcelain=v1",
                    "-z",
                    "--untracked-files=all",
                    "--no-renames",
                    "--",
                    ".",
                ])
                .output()
                && status_out.status.success()
            {
                let mut entries = Vec::new();
                for record in status_out.stdout.split(|b| *b == 0) {
                    if record.len() < 4 {
                        continue;
                    }
                    let xy = String::from_utf8_lossy(&record[..2]).into_owned();
                    let repo_rel = String::from_utf8_lossy(&record[3..]).into_owned();
                    let stamp = stat_stamp_entry(&top.join(&repo_rel));
                    entries.push((format!("{xy}:{repo_rel}"), stamp));
                }
                for dir in allowlist {
                    stat_tree_bounded(workspace, Some(dir), allowlist, STAT_LIMIT, &mut entries)?;
                }
                entries.sort();
                let mut payload = format!("git-stat-v1:{tree_oid}\n");
                for (k, v) in entries {
                    payload.push_str(&format!("{k}={v}\n"));
                }
                return Some(ToolchainPreTurnStamp {
                    digest: fingerprint::sha256(payload.as_bytes()),
                });
            }
        }
    }
    let mut entries = Vec::new();
    stat_tree_bounded(workspace, None, allowlist, STAT_LIMIT, &mut entries)?;
    entries.sort();
    let mut payload = String::from("fs-stat-v1\n");
    for (k, v) in entries {
        payload.push_str(&format!("{k}={v}\n"));
    }
    Some(ToolchainPreTurnStamp {
        digest: fingerprint::sha256(payload.as_bytes()),
    })
}

const TOOLCHAIN_MANIFEST_FILES: [&str; 8] = [
    "composer.json",
    "composer.lock",
    "package.json",
    "package-lock.json",
    "Cargo.toml",
    "pyproject.toml",
    "symfony.lock",
    "artisan",
];

pub(crate) fn hash_workspace_target(path: &Path) -> String {
    const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
    const MAX_DIR_FILES: usize = 10_000;
    const MAX_DIR_BYTES: u64 = 64 * 1024 * 1024;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return "absent".into(),
        Err(_) => return "unreadable".into(),
    };
    if crate::custody::fs::linked(&meta) {
        let target = std::fs::read_link(path)
            .map(|t| t.to_string_lossy().into_owned())
            .unwrap_or_default();
        return format!("link:{}", fingerprint::sha256(target.as_bytes()));
    }
    if meta.is_file() {
        if meta.len() > MAX_FILE_BYTES {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            return format!("file-large:{}:{mtime}", meta.len());
        }
        return match std::fs::read(path) {
            Ok(bytes) => format!("file:{}", fingerprint::sha256(&bytes)),
            Err(_) => "unreadable".into(),
        };
    }
    if !meta.is_dir() {
        return "other".into();
    }
    let mut stack = vec![path.to_path_buf()];
    let mut entries = BTreeMap::<String, String>::new();
    let mut total_bytes = 0u64;
    while let Some(dir) = stack.pop() {
        let Ok(read_dir) = std::fs::read_dir(&dir) else {
            return "unreadable".into();
        };
        let mut items: Vec<_> = read_dir.flatten().collect();
        items.sort_by_key(|entry| entry.file_name());
        for entry in items {
            let child = entry.path();
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            if executor::is_ignored_workspace_name(name_str)
                || (dir == path && (name_str == ".harness-tmp" || name_str == "target"))
            {
                continue;
            }
            let Ok(child_meta) = std::fs::symlink_metadata(&child) else {
                continue;
            };
            let Ok(rel) = child.strip_prefix(path) else {
                continue;
            };
            let rel_key = rel.to_string_lossy().replace('\\', "/");
            if crate::custody::fs::linked(&child_meta) {
                let target = std::fs::read_link(&child)
                    .map(|t| t.to_string_lossy().into_owned())
                    .unwrap_or_default();
                entries.insert(rel_key, format!("link:{target}"));
            } else if child_meta.is_dir() {
                stack.push(child);
            } else if child_meta.is_file() {
                total_bytes = total_bytes.saturating_add(child_meta.len());
                if entries.len() >= MAX_DIR_FILES || total_bytes > MAX_DIR_BYTES {
                    return "dir-overflow".into();
                }
                let digest = if child_meta.len() <= MAX_FILE_BYTES {
                    std::fs::read(&child)
                        .map(|bytes| fingerprint::sha256(&bytes))
                        .unwrap_or_else(|_| "unreadable".into())
                } else {
                    format!("len:{}", child_meta.len())
                };
                entries.insert(rel_key, format!("file:{digest}"));
            }
        }
    }
    let summary = entries
        .into_iter()
        .map(|(k, v)| format!("{k}={v}\n"))
        .collect::<String>();
    format!("dir:{}", fingerprint::sha256(summary.as_bytes()))
}

fn rel_touched_path(workspace: &Path, raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 512 {
        return None;
    }
    let path = Path::new(trimmed);
    let rel = if path.is_absolute() {
        let Ok(stripped) = path.strip_prefix(workspace) else {
            return None;
        };
        stripped.to_string_lossy().replace('\\', "/")
    } else {
        trimmed.replace('\\', "/")
    };
    let stripped = rel.trim_start_matches("./").trim_end_matches('/');
    if stripped == "." || stripped.is_empty() {
        return Some(".".to_owned());
    }
    if stripped.starts_with("..") || stripped.contains("/../") {
        return None;
    }
    Some(stripped.to_owned())
}

/// Captures tracked+dirty disk content hashes for the `ToolChain` trace's
/// touched paths and root project manifests.
pub(crate) fn toolchain_manifest_snapshot(
    workspace: &Path,
    steps: &[Value],
) -> BTreeMap<String, String> {
    let mut hashes = BTreeMap::new();
    for name in TOOLCHAIN_MANIFEST_FILES {
        hashes.insert(
            name.to_owned(),
            hash_workspace_target(&workspace.join(name)),
        );
    }
    let mut touched_paths = 0usize;
    let mut touched_root = false;
    for step in steps {
        if step["kind"] != "tool" {
            continue;
        }
        let Some(input) = step["input"].as_object() else {
            continue;
        };
        for field in [
            "file_path",
            "path",
            "dir",
            "directory",
            "TargetFile",
            "SearchPath",
            "SearchDirectory",
        ] {
            let Some(raw) = input.get(field).and_then(Value::as_str) else {
                continue;
            };
            let Some(rel) = rel_touched_path(workspace, raw) else {
                continue;
            };
            if rel == "." {
                touched_root = true;
            } else {
                touched_paths += 1;
                hashes
                    .entry(rel.clone())
                    .or_insert_with(|| hash_workspace_target(&workspace.join(&rel)));
            }
        }
    }
    if (touched_root || touched_paths == 0)
        && let Ok(tree_sha) = workspace_tree_sha(workspace)
    {
        hashes.insert(".".to_owned(), tree_sha);
    }
    hashes
}

/// Verifies whether every path recorded in `path_hashes` still matches on disk.
pub(crate) fn verify_toolchain_manifest(
    workspace: &Path,
    path_hashes: &BTreeMap<String, String>,
) -> bool {
    if path_hashes.is_empty() {
        return false;
    }
    for (rel, expected) in path_hashes {
        if rel == "." {
            if workspace_tree_sha(workspace).ok().as_deref() != Some(expected.as_str()) {
                return false;
            }
        } else if hash_workspace_target(&workspace.join(rel)) != *expected {
            return false;
        }
    }
    true
}

/// Stable 64-hex working-tree digest (committed tree plus dirty/untracked edits)
/// shared across worktrees on the same checkout state.
pub fn workspace_tree_sha(workspace: &Path) -> Result<String> {
    if let Ok(located) = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["rev-parse", "--show-toplevel", "--show-prefix"])
        .output()
        && located.status.success()
    {
        let loc_text = String::from_utf8_lossy(&located.stdout);
        let mut lines = loc_text.lines();
        if let Some(top_raw) = lines.next().map(str::trim).filter(|s| !s.is_empty()) {
            let top = PathBuf::from(top_raw);
            let prefix = lines
                .next()
                .unwrap_or("")
                .trim()
                .trim_end_matches('/')
                .to_owned();
            let tree_ref = if prefix.is_empty() {
                "HEAD^{tree}".to_owned()
            } else {
                format!("HEAD:{prefix}")
            };
            let tree_oid = std::process::Command::new("git")
                .arg("-C")
                .arg(&top)
                .args(["rev-parse", "--verify", "--quiet", &tree_ref])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "UNBORN".to_owned());
            if let Ok(status_out) = std::process::Command::new("git")
                .arg("-C")
                .arg(workspace)
                .args([
                    "-c",
                    "core.quotePath=false",
                    "status",
                    "--porcelain=v1",
                    "-z",
                    "--untracked-files=all",
                    "--no-renames",
                    "--",
                    ".",
                ])
                .output()
                && status_out.status.success()
            {
                let prefix_slash = if prefix.is_empty() {
                    String::new()
                } else {
                    format!("{prefix}/")
                };
                let mut dirty = Vec::new();
                for record in status_out.stdout.split(|b| *b == 0) {
                    if record.len() < 4 {
                        continue;
                    }
                    let xy = String::from_utf8_lossy(&record[..2]).into_owned();
                    let repo_rel = String::from_utf8_lossy(&record[3..]).into_owned();
                    let ws_rel = if prefix_slash.is_empty() {
                        repo_rel.clone()
                    } else {
                        repo_rel
                            .strip_prefix(&prefix_slash)
                            .unwrap_or(&repo_rel)
                            .to_owned()
                    };
                    let digest = hash_workspace_target(&top.join(&repo_rel));
                    dirty.push((ws_rel, xy, digest));
                }
                dirty.sort();
                let mut payload = format!("git-worktree-v1:{tree_oid}\n");
                for (ws_rel, xy, digest) in dirty {
                    payload.push_str(&format!("{xy}\t{ws_rel}\t{digest}\n"));
                }
                return Ok(fingerprint::sha256(payload.as_bytes()));
            }
        }
    }
    Ok(fingerprint::tree_sha256(workspace)?.sha256)
}

/// Conservative allowlist of strictly read-only/search tool names permitted in `ToolChain` traces.
pub fn is_read_only_tool_name(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "read"
            | "grep"
            | "glob"
            | "ls"
            | "read_file"
            | "view_file"
            | "view_file_outline"
            | "view_code_item"
            | "grep_search"
            | "find_by_name"
            | "list_dir"
            | "search"
    )
}

fn strip_workspace_prefix(workspace: &Path, ws_canon: Option<&Path>, raw: &str) -> Option<String> {
    let candidate = Path::new(raw);
    if !raw.starts_with('/') && !candidate.is_absolute() {
        return None;
    }
    if candidate
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    for root in [Some(workspace), ws_canon].into_iter().flatten() {
        if let Ok(rel) = candidate.strip_prefix(root) {
            let text = rel.to_string_lossy().replace('\\', "/");
            let trimmed = text.trim_start_matches("./").trim_end_matches('/');
            return Some(if trimmed.is_empty() {
                ".".to_owned()
            } else {
                trimmed.to_owned()
            });
        }
    }
    None
}

fn normalize_workspace_segment(
    workspace: &Path,
    ws_canon: Option<&Path>,
    segment: &str,
) -> Option<String> {
    let tokens = crate::engine::state_store::shell_tokenize(segment).ok()?;
    if tokens.is_empty() {
        return None;
    }
    let mut normalized = Vec::with_capacity(tokens.len());
    for (idx, token) in tokens.into_iter().enumerate() {
        let rewritten = if idx == 0 {
            token
        } else if let Some((flag, rhs)) = token.split_once('=')
            && !flag.is_empty()
            && (rhs.starts_with('/') || Path::new(rhs).is_absolute())
            && let Some(rel) = strip_workspace_prefix(workspace, ws_canon, rhs)
        {
            format!("{flag}={rel}")
        } else if let Some(rel) = strip_workspace_prefix(workspace, ws_canon, &token) {
            rel
        } else {
            token
        };
        normalized.push(crate::engine::state_store::shell_quote(&rewritten));
    }
    Some(normalized.join(" "))
}

fn is_read_only_shell_command(store: &SessionStore, workspace: &Path, command: &str) -> bool {
    let Some(segments) = crate::core::trace::split_read_only_shell_chain(command) else {
        return false;
    };
    let ws_canon = crate::custody::fs::canonicalize(workspace).ok();
    segments.into_iter().all(|segment| {
        let candidate = normalize_workspace_segment(workspace, ws_canon.as_deref(), &segment)
            .unwrap_or(segment);
        matches!(
            crate::engine::state_store::classify_read_only_program_command_in_store(
                Some(workspace),
                Some(store),
                &candidate,
            ),
            Ok(Some(_))
        )
    })
}

fn is_read_only_toolchain_step(store: &SessionStore, workspace: &Path, step: &Value) -> bool {
    let name = step["tool"]
        .as_str()
        .or_else(|| step["name"].as_str())
        .unwrap_or("");
    if is_read_only_tool_name(name) {
        return true;
    }
    if !crate::core::trace::is_shell_tool_name(name) {
        return false;
    }
    crate::core::trace::step_shell_command(step)
        .is_some_and(|command| is_read_only_shell_command(store, workspace, command))
}

/// Store setting JSON controlling short-turn observation admission
/// (`{"min_tool_calls": 2, "min_output_tokens": ...}`).
pub const OBSERVE_MIN_ROI: &str = "skillcache_observe_min_roi";
const DEFAULT_MIN_TOOL_CALLS: usize = 2;

pub struct TraceRecorder {
    inner: Mutex<Option<(Recorder, crate::host::usage::UsageComponents)>>,
}
impl TraceRecorder {
    pub fn new(provider: &str) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Some((
                Recorder::new(provider),
                crate::host::usage::UsageComponents::new(provider),
            ))),
        })
    }
    /// One event in the provider's own format.
    pub fn observe(&self, raw: &Value) {
        if let Ok(mut guard) = self.inner.lock()
            && let Some((recorder, usage)) = guard.as_mut()
        {
            recorder.observe(raw);
            usage.observe("main", raw);
        }
    }
    /// The recorded trace and normalized usage snapshot; a recorder gives them once.
    pub fn finish_with_usage(&self, redactor: &dyn Redactor) -> Option<(Trace, Value)> {
        let (recorder, usage) = self.inner.lock().ok()?.take()?;
        Some((recorder.finish(redactor), usage.snapshot()))
    }
    /// The recorded trace; a recorder gives it once.
    pub fn finish(&self, redactor: &dyn Redactor) -> Option<Trace> {
        self.finish_with_usage(redactor).map(|(trace, _)| trace)
    }
}

/// Feeds the recorder the raw events the run's binding observes.
pub struct Tee {
    pub primary: RunBinding,
    pub recorder: Arc<TraceRecorder>,
}
impl RunObserver for Tee {
    fn boundary(&self, signal: &str, details: &Value) -> crate::error::Result<()> {
        self.primary.boundary(signal, details)
    }
}
impl RunEvents for Tee {
    fn observe(&self, raw: Option<&Value>, normalized: Option<&Value>) -> crate::error::Result<()> {
        if let Some(raw) = raw {
            self.recorder.observe(raw);
        }
        self.primary.observe(raw, normalized)
    }
    fn enrich(&self, event: &Value) -> crate::error::Result<Value> {
        self.primary.enrich(event)
    }
}

/// What a session's trace must not keep: the values of the launch's
/// credential-like environment variables, command-line credentials, anything
/// the shared secret policy names, and the host's own paths.
#[derive(Clone)]
pub struct SessionRedactor {
    secrets: Vec<String>,
    roots: Vec<String>,
    home: Option<String>,
}
impl SessionRedactor {
    /// `environment` is what the provider process was started with.
    pub fn new(workspace: &Path, environment: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut secrets = environment
            .into_iter()
            .filter(|(name, value)| {
                value.chars().count() >= SECRET_CHARS && env_filter::withheld(name)
            })
            .map(|(_, value)| value)
            .collect::<Vec<_>>();
        // The longest first, so that a value containing another is replaced whole.
        secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
        secrets.dedup();
        Self {
            secrets,
            roots: vec![workspace.display().to_string()],
            home: run_activity::user_home(),
        }
    }
}
impl Redactor for SessionRedactor {
    fn redact(&self, text: &str) -> (String, usize) {
        let mut changed = 0;
        let mut clean = text.to_string();
        for secret in &self.secrets {
            let found = clean.matches(secret.as_str()).count();
            if found > 0 {
                changed += found;
                clean = clean.replace(secret.as_str(), "[redacted: environment]");
            }
        }
        let commands = run_activity::redact_command(&clean);
        if commands != clean {
            changed += 1;
            clean = commands;
        }
        match crate::custody::mask_secrets(&clean) {
            Ok((masked, count)) => {
                changed += count;
                clean = masked;
            }
            Err(_) => {
                return (
                    "[redacted: a possible secret could not be masked]".into(),
                    changed + 1,
                );
            }
        }
        // Host paths become workspace-relative; that is not counted as a
        // redaction, it is how the trace names files.
        (
            run_activity::strip_root(&clean, &self.roots, self.home.as_deref()),
            changed,
        )
    }
}

/// How a skill's entry document is named in a trace and a capsule: relative
/// to the workspace, or `~/...` for a skill of the user's home.
fn spelled(workspace: &Path, home: Option<&Path>, file: &Path) -> Option<String> {
    let text = |relative: &Path| {
        relative
            .components()
            .map(|part| part.as_os_str().to_str())
            .collect::<Option<Vec<_>>>()
            .map(|parts| parts.join("/"))
    };
    if let Ok(relative) = file.strip_prefix(workspace) {
        return text(relative);
    }
    let home = crate::custody::fs::canonicalize(home?).ok()?;
    file.strip_prefix(&home)
        .ok()
        .and_then(text)
        .map(|relative| format!("~/{relative}"))
}

/// How the skill whose entry document is `file` is named in a capsule, when
/// it is a skill folder of the workspace or of the user's home.
pub fn spelled_skill(workspace: &Path, file: &Path) -> Option<String> {
    let workspace = crate::custody::fs::canonicalize(workspace).ok()?;
    let home = run_activity::user_home().map(PathBuf::from);
    let file = if file.is_absolute() {
        file.to_path_buf()
    } else {
        workspace.join(file)
    };
    let file = crate::custody::fs::canonicalize(&file).ok()?;
    if file.file_name()?.to_str()? != "SKILL.md" || !file.is_file() {
        return None;
    }
    spelled(&workspace, home.as_deref(), &file)
}

/// The skill whose entry document is `file`, when it is an ordinary skill
/// folder inside the workspace or the user's home.
fn skill_entry(
    workspace: &Path,
    home: Option<&Path>,
    file: &Path,
    name: Option<&str>,
) -> Option<Value> {
    let file = crate::custody::fs::canonicalize(file).ok()?;
    if file.file_name()?.to_str()? != "SKILL.md" || !file.is_file() {
        return None;
    }
    let folder = file.parent()?;
    let path = spelled(workspace, home, &file)?;
    let tree = fingerprint::tree_sha256(folder).ok()?;
    let name = name
        .map(String::from)
        .or_else(|| folder.file_name()?.to_str().map(String::from))?;
    Some(json!({"name": name, "path": path, "tree_sha256": tree.sha256}))
}

/// Where a skill's files are, from how its path is spelled in a trace.
pub fn skill_file(workspace: &Path, path: &str) -> Option<PathBuf> {
    match path.strip_prefix("~/") {
        Some(relative) => Some(PathBuf::from(run_activity::user_home()?).join(relative)),
        None => Some(workspace.join(path)),
    }
}

fn frontmatter_name(file: &Path) -> Option<String> {
    use std::io::Read;
    // The name is in the first lines: the read is bounded, and a cut inside
    // a multi-byte character must not lose the lines before it.
    let mut bytes = Vec::new();
    std::fs::File::open(file)
        .ok()?
        .take(8192)
        .read_to_end(&mut bytes)
        .ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let rest = text
        .strip_prefix("---\r\n")
        .or_else(|| text.strip_prefix("---\n"))?;
    for line in rest.lines() {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("name:") {
            let value = value.trim().trim_matches(|ch| ch == '"' || ch == '\'');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// The skills `S` a turn used: the ones the message routed to and the ones
/// the trace shows the agent loading. Each is named with the digest of its
/// folder at this moment, so a later compile can tell that it has changed.
pub fn resolve_skills(
    workspace: &Path,
    route: &ResolvedCommand,
    message: &str,
    trace: &Trace,
) -> Vec<Value> {
    let Ok(workspace) = crate::custody::fs::canonicalize(workspace) else {
        return Vec::new();
    };
    let home = run_activity::user_home().map(PathBuf::from);
    let home = home.as_deref();
    let by_name = |name: &str| -> Option<Value> {
        // A plugin's skill (`plugin:skill`) has no folder this resolves.
        if name.is_empty() || name.contains([':', '/', '\\']) || name.starts_with('.') {
            return None;
        }
        for folder in SKILL_FOLDERS {
            if let Some(found) = [Some(workspace.as_path()), home]
                .into_iter()
                .flatten()
                .find_map(|root| {
                    skill_entry(
                        &workspace,
                        home,
                        &root.join(folder).join(name).join("SKILL.md"),
                        Some(name),
                    )
                })
            {
                return Some(found);
            }
        }
        for folder in SKILL_FOLDERS {
            for root in [Some(workspace.as_path()), home].into_iter().flatten() {
                let Ok(entries) = std::fs::read_dir(root.join(folder)) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let skill_md = entry.path().join("SKILL.md");
                    if frontmatter_name(&skill_md).as_deref() == Some(name)
                        && let Some(found) = skill_entry(&workspace, home, &skill_md, Some(name))
                    {
                        return Some(found);
                    }
                }
            }
        }
        None
    };
    let by_path = |path: &str, name: Option<&str>| -> Option<Value> {
        let file = match path.strip_prefix("~/") {
            Some(relative) => home?.join(relative),
            None if Path::new(path).is_absolute() => PathBuf::from(path),
            None => workspace.join(path),
        };
        skill_entry(&workspace, home, &file, name)
    };
    let mut skills: Vec<Value> = Vec::new();
    let mut add = |skill: Option<Value>| {
        if let Some(skill) = skill
            && skills.len() < SKILLS
            && !skills.iter().any(|known| known["path"] == skill["path"])
        {
            skills.push(skill);
        }
    };
    for request in &route.requests {
        add(request["path"]
            .as_str()
            .and_then(|path| by_path(path, request["name"].as_str())));
    }
    // A native command is sent as typed; its name is the skill it starts.
    if route.mode == "native"
        && let Some(name) = message
            .strip_prefix('/')
            .and_then(|tail| tail.split_whitespace().next())
    {
        add(by_name(name));
    }
    for SkillRef { name, path } in &trace.skills {
        add(match (name, path) {
            (_, Some(path)) => by_path(path, None),
            (Some(name), None) => by_name(name),
            (None, None) => None,
        });
    }
    skills
}

/// The turn a trace was observed in.
pub struct ObservedTurn<'a> {
    pub session: &'a Value,
    pub generation: &'a str,
    /// The message as the person sent it.
    pub message: &'a str,
    pub route: &'a ResolvedCommand,
    pub workspace: &'a Path,
    pub usage: &'a Value,
}

fn observe_min_roi(store: &SessionStore) -> (usize, Option<u64>) {
    let setting = store.setting(OBSERVE_MIN_ROI).ok().flatten();
    let min_tools = setting
        .as_ref()
        .and_then(|value| value["min_tool_calls"].as_u64())
        .and_then(|count| usize::try_from(count).ok())
        .unwrap_or(DEFAULT_MIN_TOOL_CALLS);
    let min_output = setting
        .as_ref()
        .and_then(|value| value["min_output_tokens"].as_u64());
    (min_tools, min_output)
}

/// Checks whether an observed trace meets recording eligibility without
/// writing to disk or computing workspace diffs.
pub fn check_admissible(
    store: &SessionStore,
    workspace: &Path,
    route: &ResolvedCommand,
    message: &str,
    trace: &Trace,
    usage: &Value,
) -> std::result::Result<(Vec<Value>, usize), Value> {
    let (skills, tools, _) =
        check_admissible_with_toolchain(store, false, workspace, route, message, trace, usage)?;
    Ok((skills, tools))
}

/// Checks trace admissibility, additionally admitting skillless read-only tool
/// chains when `allow_tool_chain` is true.
pub fn check_admissible_with_toolchain(
    store: &SessionStore,
    allow_tool_chain: bool,
    workspace: &Path,
    route: &ResolvedCommand,
    message: &str,
    trace: &Trace,
    usage: &Value,
) -> std::result::Result<(Vec<Value>, usize, Option<String>), Value> {
    let tools = trace
        .steps
        .iter()
        .filter(|step| step["kind"] == "tool")
        .count();
    let skills = resolve_skills(workspace, route, message, trace);
    let skipped = |reason: &str, text: &str| json!({"kind": "skillcache", "phase": "not_observed", "reason": reason, "text": text});
    if tools == 0 {
        return Err(skipped(
            "no_tool_calls",
            "SkillCache kept no trace: this turn ran no tool.",
        ));
    }
    let mut tool_chain_tree_sha = None;
    if skills.is_empty() {
        if !allow_tool_chain {
            return Err(skipped(
                "no_skill",
                "SkillCache kept no trace: no skill of this project or of your home folder was used.",
            ));
        }
        let has_mutating = trace
            .steps
            .iter()
            .filter(|step| step["kind"] == "tool")
            .any(|step| !is_read_only_toolchain_step(store, workspace, step));
        if has_mutating {
            return Err(skipped(
                "mutating_tool_chain",
                "SkillCache kept no trace: tool-chain capture admits read-only tool sequences only.",
            ));
        }
        tool_chain_tree_sha = Some(workspace_tree_sha(workspace).map_err(|err| {
            skipped(
                "storage",
                &format!("SkillCache could not fingerprint workspace tree: {err}"),
            )
        })?);
    }
    let (min_tools, min_output) = observe_min_roi(store);
    if tools < min_tools {
        return Err(skipped(
            "low_roi",
            "SkillCache kept no trace: this short turn is unlikely to pay back compilation.",
        ));
    }
    if let Some(min_output) = min_output
        && let Some(output_tokens) = usage["output_tokens"].as_u64()
        && output_tokens < min_output
    {
        return Err(skipped(
            "low_roi",
            "SkillCache kept no trace: this short turn produced too few tokens to pay back compilation.",
        ));
    }
    Ok((skills, tools, tool_chain_tree_sha))
}

/// Stores the trace of a completed turn. Returns the session event that says
/// what was kept, or why nothing was.
pub fn save(
    store: &SessionStore,
    turn: &ObservedTurn<'_>,
    trace: Trace,
    redactor: &dyn Redactor,
) -> Result<Value> {
    let allow_tool_chain = crate::engine::settings::tool_chain_enabled(store, turn.session);
    let (skills, tools, tool_chain_tree_sha) = match check_admissible_with_toolchain(
        store,
        allow_tool_chain,
        turn.workspace,
        turn.route,
        turn.message,
        &trace,
        turn.usage,
    ) {
        Ok(ok) => ok,
        Err(skipped) => return Ok(skipped),
    };
    let (request, _) = redactor.redact(turn.message);
    let mut redaction = trace.redaction();
    redaction["complete"] = trace.complete.into();
    if let Some(tree_sha) = &tool_chain_tree_sha {
        let manifest_hashes = toolchain_manifest_snapshot(turn.workspace, &trace.steps);
        redaction["trace_kind"] = "tool_chain".into();
        redaction["tool_chain_tree_sha"] = tree_sha.clone().into();
        redaction["tool_chain_manifest_hashes"] = json!(manifest_hashes);
    }
    let steps = Value::Array(trace.steps);
    let db = store.connection()?;
    db::save_trace(
        &db,
        &db::TraceRow {
            generation: turn.generation,
            session_id: turn.session["id"].as_str().unwrap_or(""),
            provider: &trace.provider,
            model: turn.session["model"].as_str(),
            request: &request,
            skill_refs: &Value::Array(skills.clone()),
            steps: &steps,
            usage: turn.usage,
            redaction: &redaction,
        },
    )?;
    let names = skills
        .iter()
        .filter_map(|skill| skill["name"].as_str())
        .collect::<Vec<_>>();
    let text = if tool_chain_tree_sha.is_some() {
        format!(
            "SkillCache kept this turn's read-only tool-chain trace ({tools} tool calls). It can be compiled into a tool-chain capsule."
        )
    } else {
        format!(
            "SkillCache kept this turn's trace ({tools} tool calls; skill: {}). It can be compiled into a capsule.",
            names.join(", ")
        )
    };
    Ok(json!({
        "kind": "skillcache",
        "phase": "observed",
        "generation": turn.generation,
        "steps": steps.as_array().map_or(0, Vec::len),
        "tool_calls": tools,
        "skills": names,
        "redaction": redaction,
        "text": text,
    }))
}

