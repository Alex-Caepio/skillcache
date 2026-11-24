//! Standalone compiler launch, sandbox boundary preparation, redaction, and result parsing.
use crate::custody;
use crate::error::{HarnessError, Result};
use crate::host::{
    process,
    provider::{self, Launch, LaunchAdapter, PreparedCommand},
    sandbox, strict_json, util,
};
use fancy_regex::Regex;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, atomic::AtomicBool},
};

const MAX_BYTES: usize = 4 * 1024 * 1024;
const CURSOR_PROMPT_BYTES: usize = 32 * 1024;

static SENSITIVE: std::sync::LazyLock<Option<regex::Regex>> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r#"-----BEGIN [A-Z ]*PRIVATE KEY-----|\bAKIA[A-Z0-9]{16}\b|\bsk-[A-Za-z0-9_-]{20,}|(?i:\b(?:password|api[_-]?key|access[_-]?token)\s*[:=]\s*['"]?[A-Za-z0-9_+/=-]{16,})"#).ok()
});

pub fn sensitive(value: &str) -> bool {
    SENSITIVE
        .as_ref()
        .is_none_or(|pattern| pattern.is_match(value))
}

pub fn private_directory(path: &Path) -> Result<()> {
    util::private_directory(path)
}

fn path_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| HarnessError::invalid("Path is not UTF-8"))
}

fn encoded(value: &Value) -> Result<String> {
    custody::json::canonical(value)
}

fn parse_json(raw: &[u8], limit: usize) -> Result<Value> {
    if raw.len() > limit {
        return Err(HarnessError::invalid("JSON input exceeds bound"));
    }
    strict_json::parse(raw, limit)
}

pub struct EvidenceLaunchAdapter {
    pub workspace: PathBuf,
}

impl LaunchAdapter for EvidenceLaunchAdapter {
    fn adapt(&self, input: &Launch, mut command: PreparedCommand) -> Result<PreparedCommand> {
        let workspace = custody::fs::canonicalize(&self.workspace)?;
        if custody::fs::canonicalize(&input.project)? != workspace {
            return Err(HarnessError::forbidden(
                "Discovery launch is outside its owned evidence workspace.",
            ));
        }
        for key in [
            "ACCELERATOR_HOME",
            "ACCELERATOR_STATE_DIR",
            "ACCELERATOR_PROJECT_DIR",
            "CONTEXT_TASK_ID",
            "CONTEXT_CAPSULE_DELIVERED",
            "CONTEXT_MEMORY_RECOVERY",
        ] {
            command.environment.remove(key);
            command.remove.push(key.into());
        }
        if command
            .environment
            .keys()
            .any(|k| k.starts_with("ACCELERATOR_"))
        {
            return Err(HarnessError::forbidden(
                "Discovery cannot inherit an attached accelerator boundary.",
            ));
        }
        command.readable = vec![];
        Ok(command)
    }
}

pub fn claude_read_rule(path: &Path) -> Result<String> {
    let text = path_string(path)?;
    let text = text
        .strip_prefix(r"\\?\")
        .unwrap_or(&text)
        .replace('\\', "/");
    let posix = match text.as_bytes() {
        [drive, b':', ..] if drive.is_ascii_alphabetic() => {
            format!("/{}{}", (*drive as char).to_ascii_lowercase(), &text[2..])
        }
        _ => text,
    };
    Ok(format!("Read(/{}/**)", posix.trim_end_matches('/')))
}

#[expect(
    clippy::too_many_arguments,
    reason = "Compatibility launch API keeps explicit per-call provider flags."
)]
pub(crate) fn apply_system_launch_flags(
    provider_name: &str,
    transport: provider::Transport,
    mode: &str,
    schema: &Value,
    schema_path: &Path,
    extra: &[PathBuf],
    read: &[PathBuf],
    argv: &mut Vec<String>,
) -> Result<()> {
    if !extra.is_empty() && !["codex", "claude", "command"].contains(&provider_name) {
        return Err(HarnessError::invalid(
            "Provider CLI cannot be granted additional service folders",
        ));
    }
    if provider_name == "codex" && transport == provider::Transport::Text {
        let mut additions = vec!["--skip-git-repo-check".into()];
        if mode == "edit" {
            for path in extra {
                additions.extend(["--add-dir".into(), path_string(path)?]);
            }
        }
        additions.extend(["--output-schema".into(), path_string(schema_path)?]);
        let at = argv.len().saturating_sub(1);
        argv.splice(at..at, additions);
    } else if provider_name == "claude" {
        if !extra.is_empty() {
            argv.push("--add-dir".into());
            for path in extra {
                argv.push(path_string(path)?);
            }
        }
        if !read.is_empty() {
            argv.push("--allowedTools".into());
            for path in read {
                argv.push(claude_read_rule(path)?);
            }
        }
        argv.extend(["--json-schema".into(), encoded(schema)?]);
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "Compatibility launch API keeps explicit per-call custody controls."
)]
pub fn prepare_launch(
    provider_name: &str,
    executable: &Path,
    root: &Path,
    prompt: &str,
    schema: &Value,
    schema_path: &Path,
    mode: &str,
    extra: &[PathBuf],
    read: &[PathBuf],
    adapter: Option<&dyn LaunchAdapter>,
) -> Result<PreparedCommand> {
    let step_mode = if mode == "edit" { "edit" } else { "plan" };
    let options = Launch {
        provider: provider_name.into(),
        executable: executable.into(),
        project: root.into(),
        prompt: prompt.into(),
        mode: step_mode.into(),
        model: None,
        session: None,
        effort: None,
        agents: false,
        count: 1,
        usd: None,
        hook_events: false,
        permissions: None,
    };
    let prepared = if provider_name == "command" {
        PreparedCommand {
            argv: vec![path_string(executable)?],
            environment: BTreeMap::new(),
            input: prompt.as_bytes().to_vec(),
            readable: vec![],
            remove: vec![],
            prompt_prefix: String::new(),
        }
    } else {
        if !extra.is_empty() && !["codex", "claude", "command"].contains(&provider_name) {
            return Err(HarnessError::invalid(
                "Provider CLI cannot be granted additional service folders",
            ));
        }
        let active_transport = provider::transport(&options);
        if provider_name == "cursor"
            && active_transport == provider::Transport::Text
            && prompt.len() > CURSOR_PROMPT_BYTES
        {
            return Err(HarnessError::invalid(
                "Cursor prompt exceeds argument byte limit; narrow plan",
            ));
        }
        let (mut argv, environment, input) = provider::command(&options)?;
        apply_system_launch_flags(
            provider_name,
            active_transport,
            step_mode,
            schema,
            schema_path,
            extra,
            read,
            &mut argv,
        )?;
        PreparedCommand {
            argv,
            environment,
            input,
            readable: vec![],
            remove: vec![],
            prompt_prefix: String::new(),
        }
    };
    if let Some(adapter) = adapter {
        adapter.adapt(&options, prepared)
    } else {
        Ok(prepared)
    }
}

pub fn shown_failure(text: &str) -> String {
    let cause = text
        .chars()
        .map(|character| if character.is_control() { ' ' } else { character })
        .take(300)
        .collect::<String>();
    if sensitive(&cause) {
        "(withheld: the message looks like a secret)".into()
    } else {
        cause
    }
}

pub fn worker_failure(raw: &[u8], provider_name: &str) -> Option<String> {
    let generic = [
        "Provider error",
        "Provider run failed",
        "Assistant request failed",
    ];
    let (mut said, mut plain) = (None, None);
    for line in raw
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
    {
        match parse_json(line, 2 * MAX_BYTES)
            .ok()
            .filter(Value::is_object)
        {
            Some(event) => {
                for normalized in provider::normalize(provider_name, &event) {
                    let failed = normalized["kind"] == "error"
                        || (normalized["kind"] == "result" && normalized["ok"] != true);
                    if let Some(text) = normalized["text"]
                        .as_str()
                        .filter(|text| failed && !text.trim().is_empty())
                        && (!generic.contains(&text) || said.is_none())
                    {
                        said = Some(text.to_string());
                    }
                }
            }
            None => plain = Some(String::from_utf8_lossy(line).trim().to_string()),
        }
    }
    Some(shown_failure(&said.or(plain)?))
}

pub fn worker_result(raw: &[u8], provider_name: &str) -> Result<Value> {
    if provider_name == "command" {
        return parse_json(raw, 2 * MAX_BYTES);
    }
    if !["codex", "claude", "cursor"].contains(&provider_name) {
        return Err(HarnessError::invalid("Unknown native worker provider"));
    }
    let mut answer = None;
    let mut terminal = 0;
    let mut failed = false;
    for line in raw
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.iter().all(u8::is_ascii_whitespace))
    {
        let event = parse_json(line, 2 * MAX_BYTES)?;
        if !event.is_object() {
            return Err(HarnessError::invalid("Invalid native worker event"));
        }
        for normalized in provider::normalize(provider_name, &event) {
            if normalized["kind"] == "error" {
                failed = true;
            }
            if normalized["kind"] == "result" {
                terminal += 1;
                failed |= normalized["ok"] != true;
                if provider_name == "claude" {
                    answer = event.get("structured_output").cloned();
                } else if provider_name == "cursor" {
                    answer = event.get("result").cloned();
                }
            } else if provider_name == "codex" && normalized["kind"] == "text" {
                if terminal > 0 {
                    failed = true;
                }
                answer = normalized.get("text").cloned();
            }
        }
    }
    if terminal != 1 || failed {
        return Err(HarnessError::unavailable(
            "Worker did not return one successful terminal result",
        ));
    }
    let answer = answer.ok_or_else(|| HarnessError::unavailable("Worker did not return JSON report"))?;
    if provider_name == "claude" {
        if !answer.is_object() {
            return Err(HarnessError::invalid(
                "Claude worker did not return structured output",
            ));
        }
        return Ok(answer);
    }
    let text = answer
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| HarnessError::unavailable("Worker did not return JSON report"))?;
    parse_json(text.as_bytes(), 2 * MAX_BYTES)
}

pub fn probe_failure(stdout: &[u8], code: i32) -> String {
    let reason = stdout
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .filter(|value| value["ok"] == false)
        .filter_map(|value| value["error"].as_str().map(str::to_string))
        .next_back();
    match reason {
        Some(reason) if reason.starts_with("The sandbox could read an original") => format!(
            "The SkillCache compiler sandbox did not keep the original folders unreadable. {reason}"
        ),
        Some(reason) => format!("The SkillCache compiler sandbox check failed: {reason}"),
        None => format!(
            "The SkillCache compiler sandbox could not start its boundary check (exit code {code}). On Linux, bubblewrap must be able to create user namespaces."
        ),
    }
}

pub fn runtime_profile(
    executable: &Path,
    original: &[PathBuf],
    workspace: &Path,
    codex: Option<PathBuf>,
    readable: &[PathBuf],
) -> Result<sandbox::Spec> {
    let mut runtime = BTreeSet::new();
    runtime.insert(sandbox::trusted_helper(workspace)?);
    runtime.insert(executable.to_path_buf());
    if let Some(parent) = executable.parent() {
        runtime.insert(parent.to_path_buf());
    }
    for parent in executable.ancestors().skip(1).take(6) {
        if let Ok(mut custody_files) = custody::publication::PublicationFiles::new(parent)
            && let Ok(Some(raw)) = custody_files.read("package.json", 64 * 1024)
            && let Ok(package) = parse_json(&raw, 64 * 1024)
            && [
                "@openai/codex",
                "@anthropic-ai/claude-code",
                "@cursor/agent",
                "cursor-agent",
            ]
            .contains(&package["name"].as_str().unwrap_or(""))
        {
            runtime.insert(parent.to_path_buf());
            break;
        }
    }
    for name in ["node", "bun"] {
        if let Some(path) = provider::find_executable(name) {
            let path = custody::fs::canonicalize(path)?;
            runtime.insert(path.clone());
            if let Some(parent) = path.parent() {
                runtime.insert(parent.to_path_buf());
            }
        }
    }
    if let Some(codex) = &codex {
        runtime.insert(codex.clone());
        if let Some(parent) = codex.parent() {
            runtime.insert(parent.to_path_buf());
        }
    }
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .ok_or_else(|| HarnessError::unavailable("Provider account home unavailable"))?;
    let mut accounts = vec![];
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    if codex_home.exists() {
        accounts.push(codex_home);
    }
    for name in [".claude", ".claude.json", ".config/cursor-agent", ".cursor"] {
        let path = home.join(name);
        if path.exists() {
            accounts.push(path);
        }
    }
    for grant in accounts.iter().chain(runtime.iter()).chain(readable.iter()) {
        for root in original {
            if grant.starts_with(root) || root.starts_with(grant) {
                return Err(HarnessError::forbidden(
                    "Discovery source overlaps provider account or runtime; use standalone CLI outside source folders",
                ));
            }
        }
    }
    Ok(sandbox::Spec {
        workspace: workspace.to_path_buf(),
        target: workspace.to_path_buf(),
        protected: original.to_vec(),
        readable: readable.to_vec(),
        accounts,
        runtime: runtime.into_iter().collect(),
        codex,
        evidence: Some(workspace.join("evidence")),
        network: true,
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "Compatibility API keeps explicit per-call custody and process controls."
)]
pub async fn run_raw_monitored(
    argv: &[String],
    root: &Path,
    environment: &BTreeMap<String, String>,
    remove: &[String],
    input: &[u8],
    timeout: u64,
    cancel: Arc<AtomicBool>,
    runner_lock: Option<Arc<std::fs::File>>,
    observer: Option<Arc<dyn process::RunObserver>>,
    mut event: impl FnMut(&[u8]) -> Result<()> + Send,
) -> Result<process::Output> {
    process::run_monitored(
        argv,
        root,
        environment,
        remove,
        input,
        process::Limits {
            seconds: Some(timeout),
            output: 2 * MAX_BYTES,
            line: 2 * MAX_BYTES,
        },
        cancel,
        runner_lock,
        observer,
        move |line| event(line),
    )
    .await
}

pub fn user_home() -> Option<String> {
    std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .ok()
        .filter(|value| !value.is_empty())
}

fn replace(pattern: &Regex, value: &str, mode: u8, literal: Option<&str>) -> Option<String> {
    let mut result = String::new();
    let mut end = 0;
    for capture in pattern.captures_iter(value) {
        let capture = capture.ok()?;
        let matched = capture.get(0)?;
        result.push_str(&value[end..matched.start()]);
        if let Some(literal) = literal {
            result.push_str(literal);
        } else {
            match mode {
                1 => {
                    result.push_str(capture.get(1)?.as_str());
                    result.push_str("[redacted]");
                }
                2 => {
                    result.push_str(capture.get(1)?.as_str());
                    result.push_str(" [redacted]");
                }
                3 => {
                    result.push_str(capture.get(1)?.as_str());
                    result.push_str("[redacted]");
                    result.push_str(capture.get(2)?.as_str());
                }
                4 => result.push_str("-p[redacted]"),
                _ => result.push_str("[redacted]"),
            }
        }
        end = matched.end();
    }
    result.push_str(&value[end..]);
    Some(result)
}

fn redactions() -> Option<&'static Vec<(Regex, u8)>> {
    static PATTERNS: OnceLock<Option<Vec<(Regex, u8)>>> = OnceLock::new();
    PATTERNS
        .get_or_init(|| {
            let value = r#"("[^"]*"|'[^']*'|[^\s"'`;|&)]+)"#;
            let sources = vec![
                (
                    r#"-----BEGIN [A-Z ]*PRIVATE KEY-----|\bAKIA[A-Z0-9]{16}\b|\bsk-[A-Za-z0-9_-]{20,}|(?i:\b(?:password|api[_-]?key|access[_-]?token)\s*[:=]\s*['"]?[A-Za-z0-9_+/=-]{16,})"#.to_string(),
                    0,
                ),
                (
                    format!(
                        r"(?i)((?<![\w-])--?(?:[a-z0-9]+[-_])*(?:password|passwd|pass|pwd|secret|token|api[-_]?key|access[-_]?key)(?:=|\s+))(?!-){value}"
                    ),
                    1,
                ),
                (
                    format!(
                        r"(?<![\w-])([A-Z0-9_]*(?:TOKEN|SECRET|PASSWORD|PASSWD|PWD|PASS|KEY|CREDENTIALS?)=){value}"
                    ),
                    1,
                ),
                (
                    format!(
                        r"(?i)(?<![\w-])([a-z0-9_]*(?:password|passwd|pwd|secret|token|api_?key|access_?key|private_?key)=){value}"
                    ),
                    1,
                ),
                (
                    r#"(?i)(\bauthorization:\s*(?:bearer|basic|token|digest)?\s*)([^\s"']+)"#
                        .into(),
                    1,
                ),
                (
                    r#"(?i)((?<!\S)(?:-H|--header)[=\s]*["']?\s*(?:x-)?(?:api[-_]?key|auth[-_]?token|access[-_]?token|private[-_]?token):\s*)([^\s"']+)"#
                        .into(),
                    1,
                ),
                (
                    r"(?i)(\bsshpass(?:\.exe)?(?:\s+-(?!p\b)\S+)*\s+-p)\s+(?!-)(\S+)".into(),
                    2,
                ),
                (
                    r"(?i)(\bredis-cli(?:\.exe)?\b[^|;&\n]*?\s-a)\s+(?!-)(\S+)".into(),
                    2,
                ),
                (
                    r"(?i)(\b(?:docker|podman)(?:\.exe)?\s+login\b[^|;&\n]*?\s-p)\s+(?!-)(\S+)"
                        .into(),
                    2,
                ),
                (r"\b(Bearer\s+)[A-Za-z0-9._~+/=-]{12,}".into(), 1),
                (r"(\b[A-Za-z][A-Za-z0-9+.-]*://[^/\s:@]+:)[^/\s@]+(@)".into(), 3),
                (r"((?<!\S)(?:-u|--user)\s+[^\s:]+:)(\S+)".into(), 1),
                (
                    r"\b(?:gh[pousr]_[A-Za-z0-9]{16,}|github_pat_[A-Za-z0-9_]{20,}|(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{8,}|xox[abposr]-[A-Za-z0-9-]{8,}|glpat-[A-Za-z0-9_-]{16,})"
                        .into(),
                    0,
                ),
                (
                    r"\beyJ[A-Za-z0-9_-]{8,}(?:\.[A-Za-z0-9_-]+){0,2}".into(),
                    0,
                ),
            ];
            sources
                .into_iter()
                .map(|(pattern, mode)| Regex::new(&pattern).ok().map(|regex| (regex, mode)))
                .collect()
        })
        .as_ref()
}

pub fn redact_command(value: &str) -> String {
    let Some(patterns) = redactions() else {
        return "[redacted]".into();
    };
    let mut value = value.to_string();
    for (pattern, mode) in patterns {
        let Some(clean) = replace(pattern, &value, *mode, None) else {
            return "[redacted]".into();
        };
        value = clean;
    }
    static ATTACHED: OnceLock<Option<(Regex, Regex)>> = OnceLock::new();
    let attached = ATTACHED.get_or_init(|| {
        Some((
            Regex::new(r"(?:^|[\s/;&|(])(?:mysql[\w-]*|mariadb[\w-]*|sshpass)(?:\.exe)?\b")
                .ok()?,
            Regex::new(r"(?<!\S)-p(?![\s-])\S+").ok()?,
        ))
    });
    match attached {
        Some((password, attached)) => match password.is_match(&value) {
            Ok(true) => replace(attached, &value, 4, None).unwrap_or_else(|| "[redacted]".into()),
            Ok(false) => value,
            Err(_) => "[redacted]".into(),
        },
        None => "[redacted]".into(),
    }
}

fn windows_path(root: &str) -> bool {
    cfg!(windows)
        || root.as_bytes().get(1) == Some(&b':')
        || root.starts_with("\\\\")
        || root.starts_with("//")
}

fn drive(value: &str) -> (String, &str) {
    if value.as_bytes().get(1) == Some(&b':') {
        return (value[..2].into(), &value[2..]);
    }
    if let Some(rest) = value.strip_prefix("//") {
        let mut parts = rest.splitn(3, '/');
        if let (Some(server), Some(share)) = (parts.next(), parts.next()) {
            let prefix = format!("//{server}/{share}");
            return (prefix.clone(), &value[prefix.len()..]);
        }
    }
    (String::new(), value)
}

fn norm(value: &str, win: bool) -> String {
    let value = if win {
        value.replace('\\', "/")
    } else {
        value.into()
    };
    let (prefix, rest) = if win {
        drive(&value)
    } else {
        (String::new(), value.as_str())
    };
    let absolute = rest.starts_with('/');
    let mut parts = vec![];
    for part in rest.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            if parts.last().is_some_and(|value| *value != "..") {
                parts.pop();
            } else if !absolute {
                parts.push(part);
            }
        } else {
            parts.push(part);
        }
    }
    let body = parts.join("/");
    if absolute {
        format!("{prefix}/{body}")
    } else if body.is_empty() {
        if prefix.is_empty() {
            ".".into()
        } else {
            prefix
        }
    } else {
        format!("{prefix}{body}")
    }
}

const MAX_STRIP_ROOT_REGEX_CACHE: usize = 128;
type StripRootRegexKey = (String, String, bool);
type CompiledStripRoot = Option<Arc<Vec<(Regex, String)>>>;

#[derive(Default)]
struct StripRootRegexCache {
    entries: BTreeMap<StripRootRegexKey, CompiledStripRoot>,
    order: VecDeque<StripRootRegexKey>,
}

fn strip_root_regex_cache() -> &'static Mutex<StripRootRegexCache> {
    static CACHE: OnceLock<Mutex<StripRootRegexCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(StripRootRegexCache::default()))
}

fn compile_strip_root_patterns(variant: &str, name: &str, win: bool) -> CompiledStripRoot {
    let escaped = if win {
        variant
            .split('/')
            .map(regex::escape)
            .collect::<Vec<_>>()
            .join(r"[\\/]+")
    } else {
        regex::escape(variant)
    };
    let mut compiled = Vec::with_capacity(6);
    for (start, inner) in [
        (
            r"(?:(?<=\s-[A-Za-z])|(?<=^-[A-Za-z])|(?<=file://))",
            format!("{name}/"),
        ),
        (
            r"(?<![\w./\\-])",
            if name == "." {
                String::new()
            } else {
                format!("{name}/")
            },
        ),
    ] {
        let start = format!("{}{start}{escaped}", if win { "(?i)" } else { "" });
        for (pattern, replacement) in [
            (format!(r#"{start}[\\/]+(?=[^\s"'`;|&<>()])"#), inner),
            (format!(r"{start}[\\/]+"), format!("{name}/")),
            (format!(r"{start}(?![\w./\\-])"), name.into()),
        ] {
            let regex = Regex::new(&pattern).ok()?;
            compiled.push((regex, replacement));
        }
    }
    Some(Arc::new(compiled))
}

fn cached_strip_root_patterns(variant: &str, name: &str, win: bool) -> CompiledStripRoot {
    let key = (variant.to_owned(), name.to_owned(), win);
    if let Ok(mut guard) = strip_root_regex_cache().lock()
        && let Some(found) = guard.entries.get(&key).cloned()
    {
        if let Some(pos) = guard.order.iter().position(|k| k == &key) {
            guard.order.remove(pos);
        }
        guard.order.push_back(key);
        return found;
    }
    let built = compile_strip_root_patterns(variant, name, win);
    if let Ok(mut guard) = strip_root_regex_cache().lock() {
        if let Some(pos) = guard.order.iter().position(|k| k == &key) {
            guard.order.remove(pos);
        } else if guard.entries.len() >= MAX_STRIP_ROOT_REGEX_CACHE
            && let Some(evict) = guard.order.pop_front()
        {
            guard.entries.remove(&evict);
        }
        guard.order.push_back(key.clone());
        guard.entries.insert(key, built.clone());
    }
    built
}

pub fn strip_root(value: &str, roots: &[String], home: Option<&str>) -> String {
    let mut result = value.to_string();
    for (root, name) in roots
        .iter()
        .map(|root| (root.as_str(), "."))
        .chain(home.into_iter().map(|home| (home, "~")))
    {
        let win = windows_path(root);
        let mut root = norm(root, win);
        while root.ends_with('/') || root.ends_with('\\') {
            root.pop();
        }
        if root.chars().count() < 3 {
            continue;
        }
        let contains = if win {
            let needle = root.replace('\\', "/").to_lowercase();
            result.replace('\\', "/").to_lowercase().contains(&needle)
        } else {
            result.contains(&root)
        };
        if !contains {
            continue;
        }
        let Some(patterns) = cached_strip_root_patterns(&root, name, win) else {
            return "[redacted]".into();
        };
        for (pattern, replacement) in patterns.iter() {
            let Some(clean) = replace(pattern, &result, 0, Some(replacement)) else {
                return "[redacted]".into();
            };
            result = clean;
        }
    }
    result
}
