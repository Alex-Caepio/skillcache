//! Standalone SQLite-backed store, provider registry, and compatibility types for SkillCache.
use crate::custody;
use crate::error::{HarnessError, Result};
use crate::host::provider::{Provider, discover_provider_sync};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};
use tokio::sync::broadcast;

/// Standalone DB connection wrapper supporting both `db.method()` and `&*store.connection()?`.
pub struct DbConn(pub Connection);

impl std::ops::Deref for DbConn {
    type Target = Connection;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for DbConn {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Minimal route representation compatible with extracted Harness SkillCache decisions.
#[derive(Clone, Debug, Default)]
pub struct ResolvedCommand {
    pub mode: String,
    pub text: String,
    pub requests: Vec<Value>,
    pub notice: Option<String>,
}

/// Optional observer hook carried by trace recording tee when running live launches.
#[derive(Clone, Default)]
pub struct RunBinding;

impl RunBinding {
    pub fn boundary(&self, _signal: &str, _details: &Value) -> Result<()> {
        Ok(())
    }
    pub fn observe(&self, _raw: Option<&Value>, _normalized: Option<&Value>) -> Result<()> {
        Ok(())
    }
    pub fn enrich(&self, event: &Value) -> Result<Value> {
        Ok(event.clone())
    }
}

/// Provider executable registry (`claude`, `codex`, `cursor`).
#[derive(Clone, Debug)]
pub struct ProviderDomain {
    pub providers: Arc<RwLock<BTreeMap<String, Provider>>>,
}

impl ProviderDomain {
    pub fn discover_sync() -> Self {
        let mut map = BTreeMap::new();
        for id in ["claude", "codex", "cursor"] {
            map.insert(id.to_owned(), discover_provider_sync(id));
        }
        Self {
            providers: Arc::new(RwLock::new(map)),
        }
    }

    pub fn from_map(map: BTreeMap<String, Provider>) -> Self {
        Self {
            providers: Arc::new(RwLock::new(map)),
        }
    }

    pub fn get(&self, id: &str) -> Result<Provider> {
        self.providers
            .read()
            .map_err(|_| HarnessError::unavailable("Provider registry lock failed."))?
            .get(id)
            .cloned()
            .ok_or_else(|| HarnessError::invalid("Choose a provider."))
    }

    pub fn listing(&self) -> Result<Vec<Value>> {
        let guard = self
            .providers
            .read()
            .map_err(|_| HarnessError::unavailable("Provider registry lock failed."))?;
        Ok(guard
            .values()
            .map(|p| {
                json!({
                    "id": p.id,
                    "name": p.name,
                    "available": p.available,
                    "executable": p.executable.as_ref().map(|e| e.display().to_string()),
                    "detail": p.detail,
                })
            })
            .collect())
    }
}

/// Lightweight pubsub stream compatible with Harness SSE event callers.
#[derive(Clone)]
pub struct StreamHub {
    tx: broadcast::Sender<Value>,
}

impl Default for StreamHub {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(256);
        Self { tx }
    }
}

impl StreamHub {
    pub fn publish(
        &self,
        kind: &str,
        session_id: Option<String>,
        project_id: Option<String>,
        mut detail: Value,
    ) {
        if let Some(obj) = detail.as_object_mut() {
            obj.entry("event".to_owned())
                .or_insert_with(|| Value::String(kind.to_owned()));
            if let Some(sid) = session_id {
                obj.entry("session_id".to_owned())
                    .or_insert_with(|| Value::String(sid));
            }
            if let Some(pid) = project_id {
                obj.entry("project_id".to_owned())
                    .or_insert_with(|| Value::String(pid));
            }
        }
        let _ = self.tx.send(detail);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.tx.subscribe()
    }
}

/// Standalone local SQLite store for SkillCache traces, capsules, gates, runs, and settings.
#[derive(Clone)]
pub struct SessionStore {
    pub state: PathBuf,
    db_path: PathBuf,
    runner_lock_path: PathBuf,
    events: StreamHub,
}

impl SessionStore {
    pub fn open(state: &Path) -> Result<Self> {
        custody::security::secure_private_dir(state)?;
        let db_path = state.join("skillcache.sqlite3");
        let runner_lock_path = state.join("runner.lock");
        let store = Self {
            state: custody::fs::canonicalize(state).unwrap_or_else(|_| state.to_path_buf()),
            db_path,
            runner_lock_path,
            events: StreamHub::default(),
        };
        let conn = store.open_raw()?;
        init_standalone_schema(&conn)?;
        crate::engine::db::initialize(&conn)?;
        drop(conn);
        let _ = custody::security::secure_private_file(&store.db_path);
        Ok(store)
    }

    fn open_raw(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )?;
        Ok(conn)
    }

    pub fn connection(&self) -> Result<DbConn> {
        self.open_raw().map(DbConn)
    }

    pub fn stream(&self) -> &StreamHub {
        &self.events
    }

    pub fn publish_event(
        &self,
        kind: &str,
        session_id: Option<String>,
        project_id: Option<String>,
        detail: Value,
    ) {
        self.events.publish(kind, session_id, project_id, detail);
    }

    pub fn runner_lock(&self) -> Arc<std::fs::File> {
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts
            .open(&self.runner_lock_path)
            .expect("open skillcache runner lock");
        Arc::new(file)
    }

    pub fn register(&self, workspace: &Path) -> Result<Value> {
        let canonical = custody::fs::canonicalize(workspace)?;
        if !canonical.is_dir() {
            return Err(HarnessError::invalid(format!(
                "Workspace directory does not exist: {}",
                canonical.display()
            )));
        }
        let path_str = canonical.to_string_lossy().into_owned();
        let edition = detect_edition(&canonical);
        let conn = self.connection()?;
        if let Some((id, stored_edition, created_at)) = conn
            .query_row(
                "SELECT id, edition, created_at FROM projects WHERE path = ?1",
                rusqlite::params![&path_str],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
        {
            return Ok(json!({
                "id": id,
                "path": path_str,
                "edition": stored_edition.unwrap_or(edition),
                "created_at": created_at,
            }));
        }
        let id = format!("proj-{}", &store_helpers::hexid()[..16]);
        let created_at = store_helpers::now();
        conn.execute(
            "INSERT INTO projects(id, path, edition, created_at) VALUES(?1, ?2, ?3, ?4)",
            rusqlite::params![&id, &path_str, &edition, &created_at],
        )?;
        Ok(json!({
            "id": id,
            "path": path_str,
            "edition": edition,
            "created_at": created_at,
        }))
    }

    pub fn project(&self, project_id: &str) -> Result<Value> {
        let conn = self.connection()?;
        let row = conn
            .query_row(
                "SELECT id, path, edition, created_at FROM projects WHERE id = ?1",
                rusqlite::params![project_id],
                |row| {
                    Ok(json!({
                        "id": row.get::<_, String>(0)?,
                        "path": row.get::<_, String>(1)?,
                        "edition": row.get::<_, Option<String>>(2)?,
                        "created_at": row.get::<_, String>(3)?,
                    }))
                },
            )
            .optional()?;
        row.ok_or_else(|| HarnessError::missing("The project is not registered."))
    }

    pub fn get(&self, session_id: &str) -> Result<Value> {
        let conn = self.connection()?;
        let row = conn
            .query_row(
                "SELECT s.id, s.project_id, p.path, s.provider, s.model, s.mode, s.options, s.skillcache, s.created_at, s.updated_at
                 FROM sessions s
                 LEFT JOIN projects p ON p.id = s.project_id
                 WHERE s.id = ?1",
                rusqlite::params![session_id],
                |row| {
                    let options_raw: String = row.get(6)?;
                    let skillcache_raw: String = row.get(7)?;
                    Ok(json!({
                        "id": row.get::<_, String>(0)?,
                        "project_id": row.get::<_, String>(1)?,
                        "path": row.get::<_, Option<String>>(2)?,
                        "provider": row.get::<_, String>(3)?,
                        "model": row.get::<_, Option<String>>(4)?,
                        "mode": row.get::<_, String>(5)?,
                        "options": serde_json::from_str::<Value>(&options_raw).unwrap_or_else(|_| json!({})),
                        "skillcache": serde_json::from_str::<Value>(&skillcache_raw).unwrap_or_else(|_| json!({})),
                        "created_at": row.get::<_, String>(8)?,
                        "updated_at": row.get::<_, String>(9)?,
                    }))
                },
            )
            .optional()?;
        row.ok_or_else(|| HarnessError::missing("The SkillCache session is not registered."))
    }

    pub fn upsert_session(&self, session: &Value) -> Result<Value> {
        let id = session["id"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("sess-{}", &store_helpers::hexid()[..16]));
        let project_id = session["project_id"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| HarnessError::invalid("Session requires project_id."))?;
        let provider = session["provider"].as_str().unwrap_or("claude");
        let model = session["model"].as_str();
        let mode = session["mode"].as_str().unwrap_or("edit");
        let options = serde_json::to_string(
            session
                .get("options")
                .filter(|v| !v.is_null())
                .unwrap_or(&json!({})),
        )?;
        let skillcache = serde_json::to_string(
            session
                .get("skillcache")
                .filter(|v| !v.is_null())
                .unwrap_or(&json!({"mode": "auto"})),
        )?;
        let now = store_helpers::now();
        let conn = self.connection()?;
        let project_path: String = conn
            .query_row(
                "SELECT path FROM projects WHERE id = ?1",
                rusqlite::params![project_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or_default();
        let status = session["status"].as_str().unwrap_or("idle");
        conn.execute(
            "INSERT INTO sessions(id, project_id, project_path, status, provider, model, mode, options, skillcache, created_at, updated_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
             ON CONFLICT(id) DO UPDATE SET
                 project_id = excluded.project_id,
                 project_path = excluded.project_path,
                 status = excluded.status,
                 provider = excluded.provider,
                 model = excluded.model,
                 mode = excluded.mode,
                 options = excluded.options,
                 skillcache = excluded.skillcache,
                 updated_at = excluded.updated_at",
            rusqlite::params![id, project_id, project_path, status, provider, model, mode, options, skillcache, now],
        )?;
        self.get(&id)
    }

    pub fn ensure_session_for_project(
        &self,
        project_id: &str,
        session_id: Option<&str>,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Result<Value> {
        if let Some(sid) = session_id
            && let Ok(existing) = self.get(sid)
        {
            return Ok(existing);
        }
        let sid = session_id
            .filter(|s| !s.trim().is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("sess-{}", &store_helpers::hexid()[..16]));
        let mut sc = json!({"mode": "auto"});
        if let Some(prov) = provider.filter(|p| !p.trim().is_empty()) {
            let mut compiler = json!({"provider": prov});
            if let Some(m) = model.filter(|m| !m.trim().is_empty()) {
                compiler["model"] = json!(m);
            }
            sc["compiler"] = compiler;
        }
        self.upsert_session(&json!({
            "id": sid,
            "project_id": project_id,
            "provider": provider.unwrap_or("claude"),
            "model": model,
            "mode": "edit",
            "options": {},
            "skillcache": sc,
        }))
    }

    pub fn setting(&self, key: &str) -> Result<Option<Value>> {
        let conn = self.connection()?;
        let raw: Option<String> = conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                rusqlite::params![key],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|text| serde_json::from_str::<Value>(&text))
            .transpose()
            .map_err(HarnessError::from)
    }

    pub fn set_setting(&self, key: &str, value: &Value) -> Result<()> {
        let conn = self.connection()?;
        let now = store_helpers::now();
        let encoded = serde_json::to_string(value)?;
        conn.execute(
            "INSERT INTO settings(key, value, updated_at) VALUES(?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            rusqlite::params![key, encoded, now],
        )?;
        Ok(())
    }
}

fn init_standalone_schema(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS projects(
            id TEXT PRIMARY KEY,
            path TEXT UNIQUE NOT NULL,
            edition TEXT,
            created_at TEXT NOT NULL
        );
        CREATE VIEW IF NOT EXISTS registered_projects AS
            SELECT id, path, edition, created_at FROM projects;
        CREATE TABLE IF NOT EXISTS sessions(
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            project_path TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT 'idle',
            provider TEXT NOT NULL,
            model TEXT,
            mode TEXT NOT NULL,
            options TEXT NOT NULL DEFAULT '{}',
            skillcache TEXT NOT NULL DEFAULT '{}',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS settings(
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );",
    )?;
    for col in [
        "ALTER TABLE sessions ADD COLUMN project_path TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE sessions ADD COLUMN status TEXT NOT NULL DEFAULT 'idle'",
    ] {
        let _ = db.execute(col, []);
    }
    Ok(())
}

fn detect_edition(workspace: &Path) -> String {
    if workspace.join("artisan").is_file() {
        "Laravel".into()
    } else if workspace.join("bin/console").is_file() || workspace.join("symfony.lock").is_file() {
        "Symfony".into()
    } else if workspace.join("composer.json").is_file() {
        "PHP Core".into()
    } else {
        "generic".into()
    }
}

pub mod store_helpers {
    use crate::error::{HarnessError, Result};
    use serde_json::Value;

    pub use super::SessionStore;

    pub fn now() -> String {
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false)
    }

    pub fn id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    pub fn hexid() -> String {
        uuid::Uuid::new_v4().simple().to_string()
    }

    pub fn sha(bytes: &[u8]) -> String {
        crate::custody::sha256(bytes)
    }

    pub fn exact(value: &Value, required: &[&str], optional: &[&str]) -> bool {
        let Some(object) = value.as_object() else {
            return false;
        };
        required.iter().all(|k| object.contains_key(*k))
            && object
                .keys()
                .all(|k| required.contains(&k.as_str()) || optional.contains(&k.as_str()))
    }

    pub fn text(value: &Value, label: &str, limit: usize) -> Result<String> {
        let Some(raw) = value.as_str() else {
            return Err(HarnessError::invalid(format!("Invalid {label}.")));
        };
        if raw.trim().is_empty()
            || raw.chars().count() > limit
            || raw.chars().any(|c| c < ' ' || c == '\u{7f}')
        {
            return Err(HarnessError::invalid(format!("Invalid {label}.")));
        }
        Ok(raw.to_owned())
    }
}

/// Minimal prompt carrier compatible with Harness `prompt::build_thin_formatter_prompt`.
pub struct PreparedPrompt {
    pub text: String,
    pub ledger: Value,
}

pub fn build_thin_formatter_prompt(
    message: &str,
    capsule_name: &str,
    results: &Value,
) -> Result<PreparedPrompt> {
    let header = format!(
        "Format the verified read-only SkillCache outputs from `{capsule_name}` concisely to answer the user's request. Do not re-run tools or invent values.\n\n"
    );
    let results_block = format!(
        "\n\nVerified capsule output JSON:\n{}\n",
        serde_json::to_string_pretty(results)?
    );
    let text = format!("{header}{message}{results_block}");
    let ledger = json!({
        "thin_formatter": true,
        "capsule": capsule_name,
        "bytes": text.len(),
    });
    Ok(PreparedPrompt { text, ledger })
}

/// Shell command tokenizer and read-only safety classifier for ToolChain traces.
pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

pub fn shell_tokenize(value: &str) -> Result<Vec<String>> {
    let mut result = vec![];
    let mut token = String::new();
    let mut quote = None;
    let mut active = false;
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        match quote {
            Some('\'') => {
                if character == '\'' {
                    quote = None;
                } else {
                    token.push(character);
                }
            }
            Some('"') => {
                if character == '"' {
                    quote = None;
                } else if character == '\\' {
                    let next = characters
                        .next()
                        .ok_or_else(|| HarnessError::invalid("No escaped character"))?;
                    if !matches!(next, '"' | '\\' | '$' | '\n') && next as u32 != 96 {
                        token.push('\\');
                    }
                    if next != '\n' {
                        token.push(next);
                    }
                } else {
                    token.push(character);
                }
            }
            _ => match character {
                '\'' | '"' => {
                    quote = Some(character);
                    active = true;
                }
                '\\' if !cfg!(windows) => {
                    token.push(
                        characters
                            .next()
                            .ok_or_else(|| HarnessError::invalid("No escaped character"))?,
                    );
                    active = true;
                }
                character if character.is_whitespace() => {
                    if active {
                        result.push(std::mem::take(&mut token));
                        active = false;
                    }
                }
                _ => {
                    token.push(character);
                    active = true;
                }
            },
        }
    }
    if quote.is_some() {
        return Err(HarnessError::invalid("No closing quotation"));
    }
    if active {
        result.push(token);
    }
    Ok(result)
}

const ALIGNED_READONLY_CLIS: &[&str] = &[
    "ava",
    "behat",
    "cat",
    "date",
    "deptrac",
    "diff",
    "echo",
    "eslint",
    "false",
    "find",
    "flake8",
    "gofmt",
    "grep",
    "head",
    "jest",
    "ls",
    "mocha",
    "mypy",
    "parallel-lint",
    "paratest",
    "pest",
    "phpcs",
    "phploc",
    "phpmd",
    "phpspec",
    "phpstan",
    "phpunit",
    "printf",
    "psalm",
    "pwd",
    "pylint",
    "pytest",
    "rg",
    "sort",
    "stylelint",
    "tail",
    "test",
    "true",
    "tsc",
    "uniq",
    "unittest",
    "vitest",
    "wc",
    "which",
];

const GENERATOR_MUTATE_FLAGS: &[&str] = &[
    "--fix",
    "--fix-dry-run=false",
    "--write",
    "-w",
    "--apply",
    "--update",
    "--alter",
    "--generate-baseline",
    "--set-baseline",
    "--update-baseline",
    "--migrate-configuration",
];

const READ_ONLY_INLINE_FLAGS: &[&str] = &["-c", "-e", "-r", "--eval", "--exec"];

fn is_absolute_arg_path(value: &str) -> bool {
    value.starts_with('/') || Path::new(value).is_absolute()
}

fn has_mutating_flag(tokens: &[String]) -> bool {
    tokens.iter().any(|arg| {
        let base = arg.split('=').next().unwrap_or(arg.as_str());
        GENERATOR_MUTATE_FLAGS.contains(&arg.as_str())
            || GENERATOR_MUTATE_FLAGS.contains(&base)
            || arg == "--fix-dry-run=false"
    })
}

fn has_cli_output_or_exec_override(tokens: &[String]) -> bool {
    let Some(cmd) = tokens.first().map(|s| {
        Path::new(s)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(s.as_str())
    }) else {
        return false;
    };
    let lower = cmd.to_ascii_lowercase();
    let args = &tokens[1..];
    match lower.as_str() {
        "sort" => args.iter().any(|arg| {
            arg == "-o"
                || arg == "--output"
                || arg.starts_with("--output=")
                || (!arg.starts_with("--") && arg.starts_with('-') && arg[1..].contains('o'))
        }),
        "rg" => args
            .iter()
            .any(|arg| arg == "--pre" || arg.starts_with("--pre=")),
        "git" => args.iter().any(|arg| {
            arg == "--output"
                || arg.starts_with("--output=")
                || arg == "--ext-diff"
                || arg == "-c"
                || arg.starts_with("-c=")
        }),
        _ => false,
    }
}

fn is_known_read_only_cli(tokens: &[String]) -> bool {
    let Some(cmd) = tokens.first().map(|s| {
        Path::new(s)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(s.as_str())
    }) else {
        return false;
    };
    if has_mutating_flag(tokens) || has_cli_output_or_exec_override(tokens) {
        return false;
    }
    let lower = cmd.to_ascii_lowercase();
    if ALIGNED_READONLY_CLIS.contains(&lower.as_str()) {
        if lower == "find" {
            return !tokens.iter().any(|a| {
                matches!(
                    a.as_str(),
                    "-delete"
                        | "-exec"
                        | "-execdir"
                        | "-ok"
                        | "-okdir"
                        | "-fprint"
                        | "-fprint0"
                        | "-fprintf"
                        | "-fls"
                )
            });
        }
        return true;
    }
    if lower == "git" {
        return tokens.get(1).is_some_and(|sub| {
            matches!(
                sub.as_str(),
                "status" | "diff" | "log" | "show" | "rev-parse"
            )
        });
    }
    if lower == "php" {
        return tokens.get(1).is_some_and(|f| f == "-l");
    }
    if lower == "composer" {
        return tokens
            .get(1)
            .is_some_and(|s| matches!(s.as_str(), "validate" | "show"));
    }
    if lower == "cargo" {
        return tokens
            .get(1)
            .is_some_and(|s| matches!(s.as_str(), "check" | "metadata"));
    }
    false
}

pub fn classify_read_only_program_command_in_store(
    _workspace: Option<&Path>,
    _store: Option<&SessionStore>,
    command: &str,
) -> std::result::Result<Option<String>, String> {
    let tokens = shell_tokenize(command).map_err(|e| e.message)?;
    let Some(runtime) = tokens.first() else {
        return Ok(None);
    };
    if runtime.is_empty() {
        return Ok(None);
    }
    let mut first_is_abs = false;
    for (idx, arg) in tokens.iter().enumerate() {
        let lower = arg.to_ascii_lowercase();
        if lower.ends_with(".cmd") || lower.ends_with(".bat") {
            return Err("argv_windows_script: Windows .cmd/.bat launchers are not admitted".into());
        }
        if idx > 0 && READ_ONLY_INLINE_FLAGS.contains(&arg.as_str()) {
            return Err("argv_inline_code: inline code flags are not admitted".into());
        }
        let path_part = arg.split_once('=').map_or(arg.as_str(), |(_, v)| v);
        if path_part.starts_with('~')
            || path_part
                .split(['/', '\\'])
                .any(|part| part == ".." || (part.starts_with('-') && part.ends_with("..")))
        {
            return Err("argv_parent_traversal: parent or home traversal is not admitted".into());
        }
        if is_absolute_arg_path(path_part) {
            if idx == 0 {
                first_is_abs = true;
            } else {
                return Err(
                    "argv_absolute_path: absolute paths are not admitted in auto rules".into(),
                );
            }
        }
    }
    if first_is_abs || has_mutating_flag(&tokens) || has_cli_output_or_exec_override(&tokens) {
        return Ok(None);
    }
    if is_known_read_only_cli(&tokens) {
        return Ok(Some(format!("readonly:program:{}", tokens.join(" "))));
    }
    Ok(None)
}

/// Optional permission helper matching Harness `permissions::mode(session)`.
pub fn session_permission_mode(session: &Value) -> Option<crate::host::provider::PermissionMode> {
    match session["options"]["permissions"]
        .as_str()
        .or_else(|| session["permissions"].as_str())
    {
        Some("auto") => Some(crate::host::provider::PermissionMode::Auto),
        Some("manual") => Some(crate::host::provider::PermissionMode::Manual),
        Some("bypass") => Some(crate::host::provider::PermissionMode::Bypass),
        _ => None,
    }
}

/// Unused-by-standalone stubs kept minimal for direct source parity.
pub fn graph_ir_absent(_state: &Path, _project_id: &str) -> Option<()> {
    None
}

#[derive(Default)]
pub struct PayloadJson(pub Value);

pub fn supported_runtime_set() -> BTreeSet<&'static str> {
    ["python3", "php", "node", "bash", "sh"].into_iter().collect()
}
