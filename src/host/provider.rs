//! Provider CLI discovery, launch construction, and normalized event dispatch.
use crate::error::{HarnessError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionMode {
    Auto,
    Manual,
    Bypass,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub available: bool,
    pub executable: Option<PathBuf>,
    pub detail: String,
}

#[derive(Clone)]
pub struct Launch {
    pub provider: String,
    pub executable: PathBuf,
    pub project: PathBuf,
    pub prompt: String,
    pub mode: String,
    pub model: Option<String>,
    pub session: Option<String>,
    pub agents: bool,
    pub count: u32,
    pub effort: Option<String>,
    pub usd: Option<f64>,
    pub hook_events: bool,
    pub permissions: Option<PermissionMode>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Text,
    ClaudeControl,
    CodexServer,
    CursorAcp,
}

pub fn transport(input: &Launch) -> Transport {
    match (
        input.provider.as_str(),
        input.permissions.filter(|_| input.mode == "edit"),
    ) {
        ("claude", Some(PermissionMode::Auto | PermissionMode::Manual)) => Transport::ClaudeControl,
        ("codex", Some(PermissionMode::Manual)) => Transport::CodexServer,
        ("cursor", Some(PermissionMode::Manual)) => Transport::CursorAcp,
        _ => Transport::Text,
    }
}

#[derive(Clone)]
pub struct PreparedCommand {
    pub argv: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub input: Vec<u8>,
    pub readable: Vec<PathBuf>,
    pub remove: Vec<String>,
    pub prompt_prefix: String,
}

pub trait LaunchAdapter: Send + Sync {
    fn adapt(&self, input: &Launch, command: PreparedCommand) -> Result<PreparedCommand>;
}

pub trait RunEvents: crate::host::process::RunObserver {
    fn observe(&self, raw: Option<&Value>, normalized: Option<&Value>) -> Result<()>;
    fn enrich(&self, event: &Value) -> Result<Value> {
        Ok(event.clone())
    }
}

pub fn argument(value: &str) -> Result<()> {
    if value.trim().is_empty() || value.starts_with('-') || value.chars().any(|c| c.is_control()) {
        return Err(HarnessError::invalid("Invalid native CLI argument."));
    }
    Ok(())
}

pub fn environment(provider: &str, agents: bool, count: u32) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    if provider == "claude" {
        env.insert(
            "CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS".into(),
            if agents {
                count.to_string()
            } else {
                "0".into()
            },
        );
    }
    env
}

pub type CommandParts = (Vec<String>, BTreeMap<String, String>, Vec<u8>);

pub fn command(input: &Launch) -> Result<CommandParts> {
    if !["claude", "codex", "cursor"].contains(&input.provider.as_str())
        || !["plan", "edit"].contains(&input.mode.as_str())
        || input.count < 1
        || input.count > 40
        || !input.project.is_dir()
        || input.prompt.trim().is_empty()
        || input.prompt.contains('\0')
    {
        return Err(HarnessError::invalid("Invalid native launch."));
    }
    for value in [&input.model, &input.effort, &input.session]
        .into_iter()
        .flatten()
    {
        argument(value)?;
    }
    if input.usd.is_some_and(|v| {
        input.provider != "claude" || !v.is_finite() || !(0.01..=1000.).contains(&v)
    }) {
        return Err(HarnessError::invalid(
            "A native USD cap is supported only for Claude.",
        ));
    }
    let exe = input
        .executable
        .to_str()
        .ok_or_else(|| HarnessError::invalid("Native executable path is not UTF-8."))?;
    let mut args = command_argv(&[exe.into()], Some(&input.provider))?;
    let mut env = environment(&input.provider, input.agents, input.count);
    let permission = input.permissions.filter(|_| input.mode == "edit");
    let transport = transport(input);
    match input.provider.as_str() {
        "claude" => {
            args.extend(
                [
                    "--print",
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--permission-mode",
                    match (input.mode.as_str(), permission) {
                        ("plan", _) => "plan",
                        (_, None) => "acceptEdits",
                        (_, Some(PermissionMode::Auto)) => "auto",
                        (_, Some(PermissionMode::Manual)) => "default",
                        (_, Some(PermissionMode::Bypass)) => "bypassPermissions",
                    },
                ]
                .map(String::from),
            );
            if permission == Some(PermissionMode::Bypass) {
                args.push("--allow-dangerously-skip-permissions".into());
            }
            if transport == Transport::ClaudeControl {
                args.extend(
                    [
                        "--input-format",
                        "stream-json",
                        "--permission-prompt-tool",
                        "stdio",
                    ]
                    .map(String::from),
                );
            }
            if input.effort.as_deref() == Some("ultracode") {
                if !input.agents {
                    return Err(HarnessError::invalid(
                        "Ultracode requires additional agents.",
                    ));
                }
                args.extend(["--allowedTools", "Workflow"].map(String::from));
            } else {
                args.push("--disallowedTools".into());
                args.extend(
                    if input.agents {
                        vec!["Workflow"]
                    } else {
                        vec!["Agent", "Task", "Workflow"]
                    }
                    .into_iter()
                    .map(String::from),
                );
            }
            if let Some(effort) = &input.effort {
                env.insert(
                    "CLAUDE_CODE_EFFORT_LEVEL".into(),
                    if effort == "ultracode" {
                        "xhigh".into()
                    } else {
                        effort.clone()
                    },
                );
                args.extend(["--effort".into(), effort.clone()]);
            }
            args.extend([
                "--settings".into(),
                json!({
                    "env": env,
                    "ultracode": input.effort.as_deref() == Some("ultracode"),
                    "disableWorkflows": input.effort.as_deref() != Some("ultracode")
                })
                .to_string(),
            ]);
            if input.agents && input.effort.as_deref() != Some("ultracode") {
                args.extend(["--allowedTools", "Agent", "Task"].map(String::from));
            }
            if let Some(session) = &input.session {
                args.extend(["--resume".into(), session.clone()]);
            }
            if input.hook_events {
                args.push("--include-hook-events".into());
            }
        }
        "codex" => {
            match (transport, permission) {
                (Transport::CodexServer, _) => {}
                (_, Some(PermissionMode::Auto)) => args.push("--approve-for-me".into()),
                (_, Some(PermissionMode::Bypass)) => {
                    args.push("--dangerously-bypass-approvals-and-sandbox".into())
                }
                _ => args.extend([
                    "--ask-for-approval".into(),
                    "never".into(),
                    "--sandbox".into(),
                    if input.mode == "plan" {
                        "read-only".into()
                    } else {
                        "workspace-write".into()
                    },
                ]),
            }
            if transport != Transport::CodexServer {
                args.extend(["--cd".into(), input.project.to_string_lossy().into()]);
            }
            let settings = if input.agents {
                vec![
                    "agents.enabled=true".into(),
                    "features.multi_agent=true".into(),
                    format!("agents.max_concurrent_threads_per_session={}", input.count),
                    format!(
                        "features.multi_agent_v2={{enabled=true,max_concurrent_threads_per_session={}}}",
                        input.count + 1
                    ),
                ]
            } else {
                vec![
                    "agents.enabled=false".into(),
                    "features.multi_agent=false".into(),
                    "features.multi_agent_v2=false".into(),
                ]
            };
            for setting in settings {
                args.extend(["-c".into(), setting]);
            }
            if let Some(effort) = &input.effort {
                args.extend([
                    "-c".into(),
                    format!("model_reasoning_effort={}", json!(effort)),
                ]);
            }
            let size = ["AGENTS.override.md", "AGENTS.md"]
                .iter()
                .find_map(|name| std::fs::symlink_metadata(input.project.join(name)).ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .unwrap_or(0);
            if size > 32768 {
                args.extend([
                    "-c".into(),
                    format!(
                        "project_doc_max_bytes={}",
                        (size + 32768).clamp(131072, 262144)
                    ),
                ]);
            }
            if transport == Transport::CodexServer {
                args.push("app-server".into());
            } else {
                args.push("exec".into());
                if input.session.is_some() {
                    args.push("resume".into());
                }
                args.push("--json".into());
            }
        }
        "cursor" => {
            args.push("--disable-auto-update".into());
            if transport != Transport::CursorAcp {
                args.extend(
                    ["--print", "--output-format", "stream-json", "--workspace"].map(String::from),
                );
                args.push(input.project.to_string_lossy().into());
                if permission == Some(PermissionMode::Bypass) {
                    args.extend(["--force", "--sandbox", "disabled"].map(String::from));
                } else {
                    args.extend(["--sandbox", "enabled"].map(String::from));
                }
                if input.mode == "plan" {
                    args.extend(["--mode", "plan"].map(String::from));
                }
                if let Some(session) = &input.session {
                    args.extend(["--resume".into(), session.clone()]);
                }
            }
        }
        _ => return Err(HarnessError::invalid("Unknown provider.")),
    }
    if let Some(usd) = input.usd {
        args.extend(["--max-budget-usd".into(), usd.to_string()]);
    }
    if let Some(model) = &input.model
        && transport != Transport::CodexServer
    {
        args.extend(["--model".into(), model.clone()]);
    }
    match (input.provider.as_str(), transport) {
        ("codex", Transport::Text) => {
            if let Some(session) = &input.session {
                args.push(session.clone());
            }
            args.push("-".into());
        }
        ("cursor", Transport::Text) => args.extend(["--".into(), input.prompt.clone()]),
        ("cursor", Transport::CursorAcp) => args.push("acp".into()),
        _ => {}
    }
    let stdin = if input.provider == "cursor" || transport != Transport::Text {
        vec![]
    } else {
        input.prompt.as_bytes().to_vec()
    };
    Ok((args, env, stdin))
}

pub fn command_with_adapter(
    input: &Launch,
    adapter: &dyn LaunchAdapter,
) -> Result<PreparedCommand> {
    let (argv, environment, bytes) = command(input)?;
    adapter.adapt(
        input,
        PreparedCommand {
            argv,
            environment,
            input: bytes,
            readable: vec![],
            remove: vec![],
            prompt_prefix: String::new(),
        },
    )
}

pub fn normalize(provider: &str, event: &Value) -> Vec<Value> {
    crate::host::activity::normalize(provider, event, true)
}

pub fn find_executable(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.is_absolute() {
        return path.is_file().then(|| path.to_path_buf());
    }
    #[cfg(windows)]
    let has_windows_extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["exe", "com", "cmd", "bat"]
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        });
    for folder in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        #[cfg(windows)]
        if !has_windows_extension {
            for ext in [".exe", ".com", "cmd", ".bat"] {
                let candidate = folder.join(format!("{name}{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
        let candidate = folder.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn discover_provider_sync(id: &str) -> Provider {
    let candidates = match id {
        "cursor" => vec!["cursor-agent", "agent"],
        other => vec![other],
    };
    let executable = candidates
        .into_iter()
        .find_map(find_executable)
        .and_then(|p| crate::custody::fs::canonicalize(p).ok());
    let name = match id {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "cursor" => "Cursor Agent",
        other => other,
    };
    Provider {
        id: id.into(),
        name: name.into(),
        available: executable.is_some(),
        detail: if executable.is_some() {
            "CLI executable located on PATH.".into()
        } else {
            "CLI executable not found on PATH.".into()
        },
        executable,
    }
}

pub fn command_argv(command: &[String], _provider: Option<&str>) -> Result<Vec<String>> {
    if command.is_empty() || command.iter().any(|s| s.contains('\0')) {
        return Err(HarnessError::invalid("A native executable is required."));
    }
    #[cfg(windows)]
    {
        let executable = find_executable(&command[0]).unwrap_or_else(|| PathBuf::from(&command[0]));
        let mut out = vec![executable.to_string_lossy().into()];
        out.extend_from_slice(&command[1..]);
        return Ok(out);
    }
    #[cfg(not(windows))]
    {
        Ok(command.to_vec())
    }
}
