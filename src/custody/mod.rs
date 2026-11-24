//! Local custody, bounded JSON, privacy redaction, git framing, and rooted publication helpers.
#![allow(dead_code)]

pub mod fs;
pub mod git;
pub mod json;
pub mod native_identity;
pub mod python_paths;
pub mod privacy;
pub mod publication;
pub mod security;
pub mod spelling;
pub mod strict_json;
pub mod unicode_fold;

use crate::error::{Result, RuntimeError};
use std::path::{Component, Path, PathBuf};

pub use privacy::{mask_secrets, secret_label};

pub fn hex_lower(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn uuid4() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn hexid() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

pub fn sha256(bytes: &[u8]) -> String {
    publication::sha256(bytes)
}

pub fn folded(value: &str) -> String {
    unicode_fold::fold(value)
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false)
}

pub fn now_epoch_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    if !out.has_root() {
                        out.push(Component::ParentDir);
                    }
                } else {
                    out.pop();
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

pub fn absolute_lexical(path: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(normalize_lexical(&joined))
}

/// Return true when `path` is a packaged `skillcache` plugin (or marketplace cache) directory.
pub fn is_plugin_dir(path: &Path) -> bool {
    (path.join(".codex-plugin/plugin.json").is_file()
        || path.join(".claude-plugin/plugin.json").is_file())
        && path.join("bin/skillcache-engine").is_file()
}

/// Return true when `start` resides inside a packaged plugin directory before reaching `.git`.
pub fn inside_plugin_dir(start: &Path) -> bool {
    let abs = absolute_lexical(start).unwrap_or_else(|_| start.to_path_buf());
    for ancestor in abs.ancestors() {
        if is_plugin_dir(ancestor) {
            return true;
        }
        if ancestor.join(".git").exists() {
            break;
        }
    }
    false
}

/// Resolve an explicit or cwd-defaulted target workspace, refusing omitted/`"."` when `cwd`
/// resides inside a packaged plugin directory (Codex `.codex-mcp.json` sets `cwd: "."`).
pub fn resolve_project_path(project: Option<&str>) -> Result<PathBuf> {
    let raw_cwd = std::env::current_dir()?;
    let canon_cwd = std::fs::canonicalize(&raw_cwd).unwrap_or_else(|_| raw_cwd.clone());
    let cwd_is_plugin = inside_plugin_dir(&raw_cwd) || inside_plugin_dir(&canon_cwd);

    let trimmed = project.map(str::trim).filter(|value| !value.is_empty());
    let is_cur_dir = matches!(trimmed, Some("." | "./." | "./"));
    let candidate = match trimmed {
        None => {
            if cwd_is_plugin {
                return Err(RuntimeError::coded(
                    crate::error::ErrorKind::Policy,
                    "plugin-cwd",
                    "Refusing omitted project path while cwd is inside the skillcache plugin directory; pass an explicit target repository path.",
                ));
            }
            canon_cwd
        }
        _ if is_cur_dir => {
            if cwd_is_plugin {
                return Err(RuntimeError::coded(
                    crate::error::ErrorKind::Policy,
                    "plugin-cwd",
                    "Refusing '.' or './.' project path while cwd is inside the skillcache plugin directory; pass an explicit target repository path.",
                ));
            }
            canon_cwd
        }
        Some(raw) => {
            let candidate = PathBuf::from(raw);
            if candidate.is_absolute() {
                candidate
            } else {
                if cwd_is_plugin {
                    return Err(RuntimeError::coded(
                        crate::error::ErrorKind::Policy,
                        "plugin-cwd",
                        "Refusing relative project path while cwd is inside the skillcache plugin directory; pass an explicit absolute project path.",
                    ));
                }
                raw_cwd.join(candidate)
            }
        }
    };

    if inside_plugin_dir(&candidate) {
        return Err(RuntimeError::coded(
            crate::error::ErrorKind::Policy,
            "plugin-cwd",
            "Refusing to operate on the skillcache plugin directory as a target project.",
        ));
    }

    let metadata = std::fs::symlink_metadata(&candidate).map_err(|_| {
        RuntimeError::not_found(format!(
            "Target project directory not found: {}",
            candidate.display()
        ))
    })?;
    if fs::linked(&metadata) || !metadata.is_dir() {
        return Err(RuntimeError::policy(
            "Target project path must be an ordinary directory, not a symlink.",
        ));
    }
    let resolved = fs::canonicalize(&candidate)?;
    if inside_plugin_dir(&resolved) {
        return Err(RuntimeError::coded(
            crate::error::ErrorKind::Policy,
            "plugin-cwd",
            "Refusing to operate on the skillcache plugin directory as a target project.",
        ));
    }
    Ok(resolved)
}
