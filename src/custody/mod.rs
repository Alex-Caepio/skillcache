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

/// Return true when `path` is a packaged `skillcache` plugin (or Antigravity/Claude/Codex extension) directory.
pub fn is_plugin_dir(path: &Path) -> bool {
    let has_manifest = path.join(".codex-plugin/plugin.json").is_file()
        || path.join(".claude-plugin/plugin.json").is_file()
        || path.join(".antigravity-plugin/plugin.json").is_file()
        || path.join(".gemini-plugin/plugin.json").is_file()
        || path.join("plugin.json").is_file()
        || path.join("mcp_config.json").is_file()
        || path.join(".mcp.json").is_file();
    let has_engine = path.join("bin/skillcache-engine").is_file()
        || path.join("bin/skillcache-engine.exe").is_file();
    has_manifest && has_engine
}

fn has_component_subseq(path: &Path, seq: &[&str]) -> bool {
    let parts: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    parts.windows(seq.len()).any(|w| {
        w.iter()
            .zip(seq.iter())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

/// Return true when a normalized path resides inside an Antigravity/Claude/Codex plugin cache directory.
pub fn looks_like_plugin_cache(path: &Path) -> bool {
    let abs = absolute_lexical(path).unwrap_or_else(|_| path.to_path_buf());
    has_component_subseq(&abs, &[".gemini", "config", "plugins"])
        || has_component_subseq(&abs, &[".antigravity", "plugins"])
        || has_component_subseq(&abs, &[".claude", "plugins", "cache"])
        || has_component_subseq(&abs, &[".codex", "plugins", "cache"])
}

/// Return true when `start` resides inside a packaged plugin directory or cache before reaching `.git`.
pub fn inside_plugin_dir(start: &Path) -> bool {
    let abs = absolute_lexical(start).unwrap_or_else(|_| start.to_path_buf());
    if looks_like_plugin_cache(&abs) {
        return true;
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn antigravity_plugin_markers_require_engine_and_cache_paths_fail_closed() {
        let tmp = TempDir::new().unwrap();

        for marker in [
            ".antigravity-plugin/plugin.json",
            ".gemini-plugin/plugin.json",
            "plugin.json",
            "mcp_config.json",
            ".mcp.json",
            ".claude-plugin/plugin.json",
            ".codex-plugin/plugin.json",
        ] {
            let root = tmp.path().join(marker.replace(['/', '.'], "_"));
            let marker_path = root.join(marker);
            std::fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
            std::fs::write(&marker_path, "{}").unwrap();
            assert!(!is_plugin_dir(&root), "{marker} alone must not trigger without bin/skillcache-engine");

            std::fs::create_dir_all(root.join("bin")).unwrap();
            std::fs::write(root.join("bin/skillcache-engine"), "").unwrap();
            assert!(is_plugin_dir(&root), "{marker} + bin/skillcache-engine must match");
            assert!(inside_plugin_dir(&root.join("nested/sub")));
        }

        let cache_dir = tmp.path().join(".gemini/config/plugins/skillcache");
        std::fs::create_dir_all(cache_dir.join("skills/compile")).unwrap();
        assert!(looks_like_plugin_cache(&cache_dir));
        assert!(inside_plugin_dir(&cache_dir));
        assert!(inside_plugin_dir(&cache_dir.join("skills/compile")));

        let normalized = tmp.path().join(".gemini/config/foo/../plugins/skillcache");
        assert!(inside_plugin_dir(&normalized));

        let agy_cache = tmp.path().join(".antigravity/plugins/skillcache");
        assert!(inside_plugin_dir(&agy_cache));
    }
}
