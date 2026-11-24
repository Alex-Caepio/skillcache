//! Warm execution: a turn that runs a qualified capsule instead of reading
//! the skill.
//!
//! A turn is warm only when everything about it is what a capsule was
//! qualified for: an editing session of Claude or Codex, a message that routes
//! to exactly one skill, one active capsule of that skill that still matches
//! its skill, its environment and its rules. Anything else is a miss, and a
//! miss changes nothing about the turn. On a hit the prompt carries the
//! capsule's description, the bindings to take from the request, the opening
//! steps and the executor's protocol; it never carries an observed value, a
//! program's code or where the package is.
use crate::engine::{
    compile::Runtime,
    db, settings, staleness_for_warm_candidate_in_project, staleness_in_project,
    state_store::{ResolvedCommand, SessionStore},
    trace,
};
use crate::error::{HarnessError, Result};
use crate::host::{
    provider::{Launch, LaunchAdapter, PreparedCommand},
    sandbox::Spec,
};
use crate::core::{
    Form, Step,
    capsule::{CapsuleKind, Network},
    compile_rules::{
        BINDING_WORKSPACE_SCAN_LIMIT, extract_deterministic_bindings, normalize_safe_relative_path,
    },
    env_filter,
    executor::is_ignored_workspace_name_with,
    package, prompt, state, trace as sc_trace,
};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

/// Removes leftover warm session state directories at server startup.
pub fn clear_sessions(state: &Path) {
    let _ = std::fs::remove_dir_all(state.join("skillcache/sessions"));
}

/// The experiment switch for choosing among several capsules of one skill by
/// their descriptions. Off until it has its own measurement.
pub const RETRIEVAL_BY_MU: &str = "skillcache.retrieval_by_mu";
/// The setting that lets a capsule's own warm turns ask for its
/// recompilation: `{window, failed_runs}`. Absent, the Harness only shows how
/// the turns went. There are no built-in thresholds, because none has been
/// measured; whoever sets them names them.
pub const RECOMPILE_THRESHOLDS: &str = "skillcache.recompile_thresholds";

/// `(window, failed_runs)` when the setting holds usable numbers.
fn recompile_thresholds(store: &SessionStore) -> Option<(i64, usize)> {
    let setting = store.setting(RECOMPILE_THRESHOLDS).ok().flatten()?;
    let window = setting["window"]
        .as_u64()
        .filter(|window| (1..=50).contains(window))?;
    let failed = setting["failed_runs"]
        .as_u64()
        .filter(|failed| (1..=window).contains(failed))?;
    Some((window as i64, failed as usize))
}

/// A turn that runs a capsule (or an evaluation ablation arm).
pub struct Warm {
    pub capsule_id: String,
    pub form: Form,
    pub eval_arm: Option<String>,
    pub name: String,
    /// What replaces the skill request in the prompt.
    pub block: String,
    /// Hybrid only: the executor's variables for the provider's process.
    pub environment: BTreeMap<String, String>,
    pub bin: Option<PathBuf>,
    pub package: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
    pub(crate) engine: Option<PathBuf>,
    pub(crate) codex: Option<PathBuf>,
    /// True when the active verified capsule is pure deterministic read-only (`Step::Program` only)
    /// and eligible for direct in-process execution without paying the ~25k prefix LLM turn.
    pub direct_dispatch: bool,
    /// Deterministically extracted bindings for parameterized direct dispatch.
    pub direct_bindings: Option<serde_json::Map<String, Value>>,
    /// The last journal line already turned into a session event.
    seen: AtomicU64,
    /// The last sidecar `run-log.changes.jsonl` line already forwarded to focus tracking.
    seen_changes: AtomicU64,
}
impl Warm {
    fn agent_only(&self) -> bool {
        self.eval_arm.as_deref() == Some("skillcache_agent")
    }

    pub(crate) fn with_engine(&self, engine: Option<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            capsule_id: self.capsule_id.clone(),
            form: self.form,
            eval_arm: self.eval_arm.clone(),
            name: self.name.clone(),
            block: self.block.clone(),
            environment: self.environment.clone(),
            bin: self.bin.clone(),
            package: self.package.clone(),
            state_dir: self.state_dir.clone(),
            engine: self.engine.clone().or(engine),
            codex: self.codex.clone(),
            direct_dispatch: self.direct_dispatch,
            direct_bindings: self.direct_bindings.clone(),
            seen: AtomicU64::new(self.seen.load(Ordering::Relaxed)),
            seen_changes: AtomicU64::new(self.seen_changes.load(Ordering::Relaxed)),
        })
    }

    #[cfg(test)]
    pub(crate) fn for_changes_test(state_dir: PathBuf) -> Self {
        Self {
            capsule_id: "c-test".into(),
            form: Form::Hybrid,
            eval_arm: None,
            name: "test".into(),
            block: String::new(),
            environment: BTreeMap::new(),
            bin: None,
            package: None,
            state_dir: Some(state_dir),
            engine: None,
            codex: None,
            direct_dispatch: false,
            direct_bindings: None,
            seen: AtomicU64::new(0),
            seen_changes: AtomicU64::new(0),
        }
    }
}

pub enum Decision {
    /// The session did not ask for warm turns.
    Off,
    Miss {
        reason: &'static str,
        text: String,
    },
    Hit(Arc<Warm>),
}
impl Decision {
    /// The session event that says what was decided.
    pub fn event(&self) -> Option<Value> {
        match self {
            Self::Off => None,
            Self::Miss { reason, text } => Some(json!({
                "kind": "skillcache",
                "phase": "miss",
                "reason": reason,
                "text": format!("SkillCache: this turn runs cold ({text})."),
            })),
            Self::Hit(warm) => {
                let mut event = json!({
                    "kind": "skillcache",
                    "phase": "hit",
                    "capsule_id": (!warm.capsule_id.is_empty()).then_some(warm.capsule_id.as_str()),
                    "form": warm.form.as_str(),
                    "mu": warm.name,
                    "text": if warm.eval_arm.as_deref() == Some("raw_trace") {
                        "SkillCache: injected observed trace (`raw_trace` evaluation arm).".to_string()
                    } else {
                        format!(
                            "SkillCache: capsule `{}` runs this request instead of the skill being read again.",
                            warm.name
                        )
                    },
                });
                if let Some(arm) = &warm.eval_arm {
                    event["eval_arm"] = arm.clone().into();
                }
                Some(event)
            }
        }
    }
    /// What the launch record keeps of the decision.
    pub fn record(&self) -> Option<Value> {
        match self {
            Self::Off => None,
            Self::Miss { reason, .. } => Some(json!({"decision": "miss", "reason": reason})),
            Self::Hit(warm) => {
                let mut record = json!({
                    "decision": "hit",
                    "capsule_id": (!warm.capsule_id.is_empty()).then_some(warm.capsule_id.as_str()),
                    "form": warm.form.as_str(),
                });
                if let Some(arm) = &warm.eval_arm {
                    record["eval_arm"] = arm.clone().into();
                }
                Some(record)
            }
        }
    }
}

fn miss(reason: &'static str, text: impl Into<String>) -> Decision {
    Decision::Miss {
        reason,
        text: text.into(),
    }
}

/// The engine under the two names a warm session calls, in a folder of the
/// Harness state that goes first on the provider's `PATH`. Copies, so that
/// their mode is their own; one set per engine build.
pub fn stage_tools(runtime: &Runtime) -> Result<PathBuf> {
    stage_tools_for(runtime, true)
}

fn stage_tools_for(runtime: &Runtime, include_run: bool) -> Result<PathBuf> {
    let engine = match &runtime.engine {
        Some(engine) => engine.clone(),
        None => std::env::current_exe()?,
    };
    let digest = runtime.engine_digest();
    if digest.len() < 16 {
        return Err(HarnessError::unavailable(
            "The engine cannot be identified.",
        ));
    }
    let folder = if include_run {
        "skillcache/bin"
    } else {
        "skillcache/bin-tool"
    };
    let root = runtime.store.state.join(folder);
    let bin = root.join(&digest[..16]);
    crate::custody::fs::ensure_directory(&bin, true)?;
    let name = |tool: &str| bin.join(format!("{tool}{}", std::env::consts::EXE_SUFFIX));
    let stage = |target: &Path| -> Result<()> {
        if !target.is_file() {
            let staging = bin.join(format!(".staging-{}", crate::engine::state_store::store_helpers::hexid()));
            #[cfg(unix)]
            let linked = std::os::unix::fs::symlink(&engine, &staging).is_ok();
            #[cfg(not(unix))]
            let linked = false;
            if !linked {
                std::fs::copy(&engine, &staging)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o500))?;
                }
            }
            std::fs::rename(&staging, target)?;
        }
        Ok(())
    };
    let tool = name("skillcache-tool");
    if include_run {
        let first = name("skillcache-run");
        stage(&first)?;
        if std::fs::symlink_metadata(&first).is_ok_and(|meta| meta.file_type().is_symlink())
            || (!tool.is_file() && std::fs::hard_link(&first, &tool).is_err())
        {
            stage(&tool)?;
        }
    } else {
        stage(&tool)?;
    }
    // An older build's copies are of no use to anyone.
    for entry in std::fs::read_dir(&root).into_iter().flatten().flatten() {
        if entry.path() != bin {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    Ok(bin)
}

fn protocol(skill: &str) -> String {
    format!(
        "How to run it:\n\
         - Start with `skillcache-run run '{{\"name\": value, ...}}'`: one JSON object, as one shell argument, with the values above. If quoting is awkward, run `skillcache-run run - <<'EOF'` with the JSON on standard input.\n\
         - Every answer is one JSON object. Do what its `status` says:\n\
         \x20 - `needs_bindings`: supply the named values with `skillcache-run continue '{{...}}'`.\n\
         \x20 - `your_steps`: do those steps yourself with your normal tools, then `skillcache-run continue '{{...}}'` with what they produce.\n\
         \x20 - `program_failed`: do that one step yourself with your normal tools, following the source skill at `{skill}`. Do not repair or re-run the program. Then `skillcache-run continue '{{...}}'` with its `required_outputs`. The rest of the plan stays compiled.\n\
         \x20 - `complete`: the plan is done; `result` holds what the programs found. Answer from it.\n\
         - If the initial bindings were wrong and the run is not finished, `skillcache-run reset` clears state so you can restart with `skillcache-run run`.\n\
         - Do not start the programs yourself and do not look for the capsule's files.\n\
         - If `skillcache-run` itself cannot be started, read `{skill}` and follow the skill as usual.\n"
    )
}

/// What a hit puts in the prompt in place of the skill request.
fn block(stored: &package::Package, skill_path: &str) -> String {
    let capsule = &stored.capsule;
    let mut text = format!(
        "\n\nSkillCache capsule for this request: {} — {}\n",
        capsule.mu.name, capsule.mu.description
    );
    if let Some(markdown) = &stored.markdown {
        text.push_str(&format!(
            "This written procedure was compiled from the skill at `{skill_path}`. Follow it instead of reading the skill; read the skill only where the procedure does not cover what you meet.\n\n{}\n",
            markdown.trim()
        ));
        return text;
    }
    text.push_str(&format!(
        "The skill at `{skill_path}` was compiled into a short plan. Do not read the skill first: a local executor runs the plan's programs, and you do only what it hands back to you.\n"
    ));
    if !capsule.bindings.is_empty() {
        text.push_str("\nValues to take from the request before you start:\n");
        for binding in &capsule.bindings {
            text.push_str(&format!(
                "- `{}` ({}){}\n",
                binding.name,
                binding.kind.as_str(),
                binding
                    .description
                    .as_deref()
                    .map(|description| format!(": {description}"))
                    .unwrap_or_default()
            ));
        }
    }
    let opening = capsule
        .steps
        .iter()
        .take_while(|step| matches!(step, Step::Instruction { .. }))
        .collect::<Vec<_>>();
    if !opening.is_empty() {
        text.push_str("\nYour opening steps (do them yourself, then start the executor with what they produce as well):\n");
        for (index, step) in opening.iter().enumerate() {
            if let Step::Instruction {
                text: instruction,
                source,
                produces,
            } = step
            {
                text.push_str(&format!("{}. {instruction}", index + 1));
                if let Some(source) = source {
                    text.push_str(&format!(
                        " (source: `{}`{})",
                        source.path,
                        source
                            .section
                            .as_deref()
                            .map(|section| format!(", section \"{section}\""))
                            .unwrap_or_default()
                    ));
                }
                if !produces.is_empty() {
                    text.push_str(&format!(" Produces: {}.", produces.join(", ")));
                }
                text.push('\n');
            }
        }
    }
    text.push('\n');
    text.push_str(&protocol(skill_path));
    text
}

/// Deterministic fallback when multiple eligible active capsules match one skill:
/// lowest smoothed recent post-requalify failure/incomplete rate, then hybrid form,
/// then newest update.
fn best_by_health_and_recency(store: &SessionStore, candidates: &[Value]) -> Value {
    let db = store.connection().ok();
    let health_rate = |capsule_id: &str| -> f64 {
        let (total, unhealthy) = db
            .as_ref()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT COUNT(*),
                            COALESCE(SUM(CASE WHEN deopts > 0 OR outcome IN ('unfinished','not_started') THEN 1 ELSE 0 END), 0)
                     FROM skillcache_runs
                     WHERE capsule_id=?1
                       AND rowid>COALESCE((SELECT runs_reset_rowid FROM skillcache_capsules WHERE capsule_id=?1),0)",
                    [capsule_id],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .ok()
            })
            .unwrap_or((0, 0));
        (unhealthy as f64 + 0.5) / (total as f64 + 3.0)
    };
    candidates
        .iter()
        .min_by(|a, b| {
            let ra = health_rate(a["capsule_id"].as_str().unwrap_or(""));
            let rb = health_rate(b["capsule_id"].as_str().unwrap_or(""));
            let ha = a["form"] != "hybrid";
            let hb = b["form"] != "hybrid";
            let ua = a["updated_at"].as_str().unwrap_or("");
            let ub = b["updated_at"].as_str().unwrap_or("");
            let ida = a["capsule_id"].as_str().unwrap_or("");
            let idb = b["capsule_id"].as_str().unwrap_or("");
            ra.total_cmp(&rb)
                .then_with(|| ha.cmp(&hb))
                .then_with(|| ub.cmp(ua))
                .then_with(|| ida.cmp(idb))
        })
        .cloned()
        .unwrap_or_else(|| candidates[0].clone())
}

fn is_read_only_capsule(capsule_row: &Value) -> bool {
    if capsule_row["form"] == "textual" {
        return true;
    }
    if let Some(read_only) = capsule_row["is_read_only"].as_bool() {
        return read_only;
    }
    let warm_dir = PathBuf::from(capsule_row["artifact_dir"].as_str().unwrap_or("")).join("warm");
    super::cached_package_read_only(&warm_dir).unwrap_or(false)
}

/// Among several active capsules of one skill, the one whose description
/// matches the request best. Used only behind [`RETRIEVAL_BY_MU`] and tool-chain ranking.
fn best_by_description(store: &SessionStore, ids: &[String], request: &str) -> Option<String> {
    let words = request
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| word.chars().count() >= 3)
        .filter(|word| !is_mu_stopword(&word.to_ascii_lowercase()))
        .take(24)
        .map(|word| format!("\"{}\"", word.replace('"', "")))
        .collect::<Vec<_>>();
    if words.is_empty() {
        return None;
    }
    let db = store.connection().ok()?;
    let mut query = db
        .prepare(
            "SELECT capsule_id FROM skillcache_capsules_fts
             WHERE skillcache_capsules_fts MATCH ?1
             ORDER BY bm25(skillcache_capsules_fts)",
        )
        .ok()?;
    let ranked = query
        .query_map(rusqlite::params![words.join(" OR ")], |row| {
            row.get::<_, String>(0)
        })
        .ok()?
        .flatten()
        .collect::<Vec<_>>();
    ranked.into_iter().find(|id| ids.contains(id))
}

/// Decides whether this turn runs a capsule. Never fails the turn: whatever
/// cannot be established is a miss with its reason.
pub fn decide(
    runtime: &Runtime,
    session: &Value,
    route: &ResolvedCommand,
    message: &str,
    workspace: &Path,
    generation: &str,
) -> Decision {
    decide_for(
        runtime,
        session,
        route,
        message,
        workspace,
        Some(generation),
    )
}

/// What [`decide`] would say for a message that has not been sent. Nothing is
/// staged and no capsule changes its status: a preview has no effect.
pub fn preview(
    runtime: &Runtime,
    session: &Value,
    route: &ResolvedCommand,
    message: &str,
    workspace: &Path,
) -> Value {
    let decision = decide_for(runtime, session, route, message, workspace, None);
    let mode = settings::mode(session);
    match &decision {
        Decision::Off => json!({"mode": mode, "decision": "off"}),
        Decision::Miss { reason, text } => {
            json!({"mode": mode, "decision": "miss", "reason": reason, "text": text})
        }
        Decision::Hit(warm) => {
            let mut answer = json!({
                "mode": mode,
                "decision": "hit",
                "capsule": if warm.capsule_id.is_empty() {
                    Value::Null
                } else {
                    json!({
                        "capsule_id": warm.capsule_id,
                        "mu": warm.name,
                        "form": warm.form.as_str(),
                    })
                },
            });
            if let Some(arm) = &warm.eval_arm {
                answer["eval_arm"] = arm.clone().into();
            }
            answer
        }
    }
}

const RAW_TRACE_VIEW_BYTES: usize = 128 * 1024;

fn capsule_matches_arm(capsule: &Value, arm: Option<&str>) -> bool {
    let cap_arm = capsule["eval_arm"].as_str();
    match arm {
        None => cap_arm.is_none(),
        Some("written_procedure") => cap_arm == Some("written_procedure"),
        Some("skillcache_no_replay") => cap_arm == Some("skillcache_no_replay"),
        Some("skillcache_agent" | "skillcache_executor") => cap_arm.is_none() || cap_arm == arm,
        Some(_) => false,
    }
}

fn decide_raw_trace(
    runtime: &Runtime,
    session: &Value,
    route: &ResolvedCommand,
    workspace: &Path,
) -> Decision {
    let project_id = session["project_id"].as_str().unwrap_or("");
    let db = match runtime.store.connection() {
        Ok(db) => db,
        Err(error) => return miss("unavailable", error.to_string()),
    };
    let stored = if let Some(trace_gen) = settings::trace_generation(session) {
        let trace = match db::trace(&db, trace_gen) {
            Ok(trace) => trace,
            Err(error) => return miss("unavailable", error.to_string()),
        };
        match trace {
            Some(trace) => {
                let trace_sid = trace["session_id"].as_str().unwrap_or("");
                let trace_project = db
                    .query_row(
                        "SELECT project_id FROM sessions WHERE id=?1",
                        [trace_sid],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .ok()
                    .flatten();
                if trace_project.as_deref() != Some(project_id) {
                    None
                } else {
                    Some(trace)
                }
            }
            None => None,
        }
    } else if route.requests.len() == 1 {
        let request = &route.requests[0];
        let skill_name = request["name"].as_str().unwrap_or("");
        let skill_path = request["path"]
            .as_str()
            .and_then(|path| trace::spelled_skill(workspace, Path::new(path)));
        let found = if !skill_name.is_empty() {
            db::latest_trace_for_skill(&db, project_id, skill_name)
        } else {
            Ok(None)
        };
        match found {
            Ok(Some(trace)) => Some(trace),
            Ok(None) => match skill_path {
                Some(path) => match db::latest_trace_for_skill(&db, project_id, &path) {
                    Ok(trace) => trace,
                    Err(error) => return miss("unavailable", error.to_string()),
                },
                None => None,
            },
            Err(error) => return miss("unavailable", error.to_string()),
        }
    } else {
        return miss(
            "no_single_skill",
            "the message names no skill and no `trace_generation` was given",
        );
    };
    let Some(stored) = stored else {
        return miss(
            "no_trace",
            "no observed trace exists for `raw_trace` evaluation",
        );
    };
    let steps = stored["steps"].as_array().cloned().unwrap_or_default();
    let view = serde_json::to_string_pretty(&sc_trace::compiler_view(&steps, RAW_TRACE_VIEW_BYTES))
        .unwrap_or_else(|_| "[]".into());
    let name = stored["skill_refs"]
        .as_array()
        .and_then(|skills| skills.first())
        .and_then(|skill| skill["name"].as_str())
        .unwrap_or("raw_trace")
        .to_string();
    let block = format!(
        "\n\nObserved trace reference for this request (`raw_trace` evaluation arm):\n\
         Prior request: {}\n\
         Credential-redacted observed trace:\n```json\n{}\n```\n\
         Complete the request with your normal tools using this trace as reference.\n",
        stored["request"].as_str().unwrap_or(""),
        view
    );
    Decision::Hit(Arc::new(Warm {
        capsule_id: String::new(),
        form: Form::Textual,
        eval_arm: Some("raw_trace".into()),
        name,
        block,
        environment: BTreeMap::new(),
        bin: None,
        package: None,
        state_dir: None,
        engine: None,
        codex: None,
        direct_dispatch: false,
        direct_bindings: None,
        seen: AtomicU64::new(0),
        seen_changes: AtomicU64::new(0),
    }))
}

fn is_mu_stopword(tok: &str) -> bool {
    matches!(
        tok,
        "the"
            | "and"
            | "for"
            | "with"
            | "from"
            | "this"
            | "that"
            | "are"
            | "was"
            | "were"
            | "can"
            | "could"
            | "would"
            | "should"
            | "into"
            | "onto"
            | "about"
            | "please"
            | "how"
            | "what"
            | "when"
            | "where"
            | "which"
            | "who"
            | "why"
            | "you"
            | "your"
            | "our"
            | "out"
            | "not"
            | "but"
            | "all"
            | "any"
            | "has"
            | "have"
            | "had"
            | "did"
            | "does"
            | "its"
            | "let"
            | "use"
            | "using"
            | "via"
    )
}

fn tokenize_mu(text: &str) -> BTreeSet<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|tok| tok.chars().count() >= 3)
        .map(str::to_lowercase)
        .filter(|tok| !is_mu_stopword(tok))
        .collect()
}

fn mu_tokens_for_capsule(capsule: &Value) -> BTreeSet<String> {
    let mu_name = capsule["mu"]["name"].as_str().unwrap_or("");
    let desc = capsule["mu"]["description"].as_str().unwrap_or("");
    tokenize_mu(&format!("{mu_name} {desc}"))
}

fn capsule_lexical_f1_millionths(msg_tokens: &BTreeSet<String>, capsule: &Value) -> u64 {
    if msg_tokens.is_empty() {
        return 0;
    }
    let mu_tokens = mu_tokens_for_capsule(capsule);
    if mu_tokens.is_empty() {
        return 0;
    }
    let overlap = msg_tokens
        .iter()
        .filter(|tok| mu_tokens.contains(*tok))
        .count() as u64;
    let denom = (msg_tokens.len() + mu_tokens.len()) as u64;
    (2 * overlap * 1_000_000) / denom.max(1)
}

fn message_matches_mu(message: &str, capsule: &Value, hint: Option<&str>) -> bool {
    let mu_name = capsule["mu"]["name"].as_str().unwrap_or("").trim();
    if let Some(hint) = hint
        && hint.eq_ignore_ascii_case(mu_name)
    {
        return true;
    }
    if !mu_name.is_empty() && message.trim().eq_ignore_ascii_case(mu_name) {
        return true;
    }
    let msg_tokens = tokenize_mu(message);
    if msg_tokens.is_empty() {
        return false;
    }
    let mu_tokens = mu_tokens_for_capsule(capsule);
    if mu_tokens.is_empty() {
        return false;
    }
    let overlap = msg_tokens
        .iter()
        .filter(|tok| mu_tokens.contains(*tok))
        .count();
    let union = msg_tokens.len() + mu_tokens.len() - overlap;
    // Require multi-token overlap, >=60% query-token precision, and >=33.3% Jaccard similarity
    // so broad edit requests sharing 1-2 generic tokens cannot hijack a specialized tool-chain.
    overlap >= 2 && overlap * 5 >= msg_tokens.len() * 3 && overlap * 3 >= union
}

fn decide_toolchain_candidate(
    runtime: &Runtime,
    session: &Value,
    provider: &str,
    arm: Option<&str>,
    plan_only: bool,
    workspace: &Path,
    message: &str,
) -> std::result::Result<(Value, String), Decision> {
    let project_id = session["project_id"].as_str().unwrap_or("");
    let common_dir = db::canonical_git_common_dir(workspace);
    let candidates = match runtime.store.connection().and_then(|db| {
        let siblings = db::sibling_worktree_projects(&db, project_id, workspace)?;
        match arm {
            None => db::active_toolchain_capsules_for_worktrees(
                &db,
                project_id,
                &siblings,
                common_dir.as_deref(),
                None,
            ),
            Some("written_procedure" | "skillcache_no_replay") => {
                db::active_toolchain_capsules_for_worktrees(
                    &db,
                    project_id,
                    &siblings,
                    common_dir.as_deref(),
                    arm,
                )
            }
            Some("skillcache_agent" | "skillcache_executor") => {
                let dedicated = db::active_toolchain_capsules_for_worktrees(
                    &db,
                    project_id,
                    &siblings,
                    common_dir.as_deref(),
                    arm,
                )?;
                Ok(if dedicated.is_empty() {
                    db::active_toolchain_capsules_for_worktrees(
                        &db,
                        project_id,
                        &siblings,
                        common_dir.as_deref(),
                        None,
                    )?
                } else {
                    dedicated
                })
            }
            Some(_) => Ok(Vec::new()),
        }
    }) {
        Ok(rows) => rows,
        Err(error) => return Err(miss("unavailable", error.to_string())),
    };
    let has_sibling = candidates.iter().any(|c| c["project_id"] != project_id);
    let now_tree_sha = if has_sibling {
        trace::workspace_tree_sha(workspace).ok()
    } else {
        None
    };
    let candidates: Vec<Value> = candidates
        .into_iter()
        .filter(|capsule| {
            if capsule["project_id"] == project_id {
                return true;
            }
            let cap_tree = capsule["tool_chain_tree_sha"].as_str().unwrap_or("");
            !cap_tree.is_empty()
                && now_tree_sha.as_deref() == Some(cap_tree)
                && staleness_in_project(
                    Some(&runtime.store.state),
                    Some(project_id),
                    capsule,
                    workspace,
                )
                .is_none()
        })
        .collect();
    let hint = settings::skill_hint(session);
    let matching: Vec<Value> = candidates
        .into_iter()
        .filter(|capsule| message_matches_mu(message, capsule, hint))
        .collect();
    if matching.is_empty() {
        return Err(miss("no_single_skill", "the message names no skill"));
    }
    let has_any = !matching.is_empty();
    let eligible: Vec<Value> = if provider == "cursor" {
        matching
            .into_iter()
            .filter(|capsule| capsule["form"] == "textual")
            .collect()
    } else {
        matching
    };
    if eligible.is_empty() {
        if has_any && provider == "cursor" {
            return Err(miss(
                "provider",
                "Cursor runs textual capsules only; no active textual tool-chain capsule exists",
            ));
        }
        return Err(miss("no_single_skill", "the message names no skill"));
    }
    let had_provider_eligible = !eligible.is_empty();
    let eligible: Vec<Value> = if plan_only {
        eligible.into_iter().filter(is_read_only_capsule).collect()
    } else {
        eligible
    };
    if eligible.is_empty() {
        if plan_only && had_provider_eligible {
            return Err(miss(
                "plan_mode",
                "a capsule's programs write, and this is a read-only turn",
            ));
        }
        return Err(miss("no_single_skill", "the message names no skill"));
    }
    let fresh: Vec<Value> = eligible
        .iter()
        .filter(|capsule| {
            staleness_in_project(
                Some(&runtime.store.state),
                Some(project_id),
                capsule,
                workspace,
            )
            .is_none()
        })
        .cloned()
        .collect();
    let base_pool = if fresh.is_empty() { &eligible } else { &fresh };
    let local_pool: Vec<Value> = base_pool
        .iter()
        .filter(|capsule| capsule["project_id"] == project_id)
        .cloned()
        .collect();
    let pool = if local_pool.is_empty() {
        base_pool
    } else {
        &local_pool
    };
    let chosen = match pool.len() {
        1 => pool[0].clone(),
        _ => {
            let ids = pool
                .iter()
                .filter_map(|capsule| capsule["capsule_id"].as_str().map(String::from))
                .collect::<Vec<_>>();
            if let Some(best_id) = best_by_description(&runtime.store, &ids, message)
                && let Some(found) = pool
                    .iter()
                    .find(|capsule| capsule["capsule_id"] == best_id.as_str())
                    .cloned()
            {
                found
            } else {
                let msg_tokens = tokenize_mu(message);
                let max_score = pool
                    .iter()
                    .map(|c| capsule_lexical_f1_millionths(&msg_tokens, c))
                    .max()
                    .unwrap_or(0);
                let top_scored: Vec<Value> = pool
                    .iter()
                    .filter(|c| capsule_lexical_f1_millionths(&msg_tokens, c) == max_score)
                    .cloned()
                    .collect();
                best_by_health_and_recency(&runtime.store, &top_scored)
            }
        }
    };
    Ok((chosen, String::new()))
}

fn resolve_native_slash_skill(workspace: &Path, message: &str) -> Option<(String, String)> {
    let trimmed = message.trim();
    let rest = trimmed.strip_prefix('/')?;
    let raw_cmd = rest
        .split(char::is_whitespace)
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    if raw_cmd.contains("..") || raw_cmd.contains('\\') || raw_cmd.starts_with('.') {
        return None;
    }
    let skill_name = raw_cmd.rsplit(':').next().unwrap_or(raw_cmd);
    if !skill_name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return None;
    }
    let home = crate::host::compiler_launch::user_home().map(PathBuf::from);
    for root in [Some(workspace), home.as_deref()].into_iter().flatten() {
        for folder in [".claude/skills", ".agents/skills", ".cursor/skills"] {
            let candidate = root.join(folder).join(skill_name).join("SKILL.md");
            if std::fs::symlink_metadata(&candidate).is_ok_and(|m| m.is_file())
                && let Some(spelled) = trace::spelled_skill(workspace, &candidate)
            {
                return Some((skill_name.to_owned(), spelled));
            }
        }
    }
    None
}

fn extract_route_args(
    route: &ResolvedCommand,
    message: &str,
    skill_name: Option<&str>,
) -> Option<String> {
    if let Some(args) = route
        .requests
        .first()
        .and_then(|req| req.get("args"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(args.to_owned());
    }
    let msg = message.trim();
    let name = route
        .requests
        .first()
        .and_then(|req| req["name"].as_str())
        .filter(|n| !n.is_empty())
        .or_else(|| skill_name.filter(|n| !n.is_empty()));
    if let Some(name) = name {
        for prefix in ["/", "$"] {
            let trigger = format!("{prefix}{name}");
            if let Some(rest) = msg.strip_prefix(&trigger)
                && rest.starts_with(char::is_whitespace)
            {
                let trimmed = rest.trim();
                return (!trimmed.is_empty()).then(|| trimmed.to_owned());
            }
        }
    }
    None
}

/// Resolves deterministic capsule bindings against `message`, `route`, and
/// bounded workspace inspection, failing closed (`None`) on any ambiguity.
fn resolve_host_deterministic_bindings(
    capsule: &crate::core::SkillCapsule,
    route: &ResolvedCommand,
    message: &str,
    workspace: &Path,
) -> Option<serde_json::Map<String, Value>> {
    let fallback_name = capsule.skills.first().map(|s| s.name.as_str());
    let route_args = extract_route_args(route, message, fallback_name);
    let allowlist = &capsule.capabilities.allow_ignored;
    extract_deterministic_bindings(capsule, message, route_args.as_deref(), |re_opt, tokens| {
        let ws_canon = crate::custody::fs::canonicalize(workspace).map_err(|_| ())?;
        let check_rel = |candidate: &str| -> Option<String> {
            let norm = normalize_safe_relative_path(candidate)?;
            let first = norm.split('/').next()?;
            if is_ignored_workspace_name_with(first, allowlist)
                || first == ".harness-tmp"
                || first == "target"
            {
                return None;
            }
            let full = ws_canon.join(&norm);
            let meta = std::fs::symlink_metadata(&full).ok()?;
            if crate::custody::fs::linked(&meta) {
                return None;
            }
            let canon = crate::custody::fs::canonicalize(&full).ok()?;
            if !canon.starts_with(&ws_canon) || canon == ws_canon {
                return None;
            }
            if re_opt.is_some_and(|re| !re.is_match(&norm)) {
                return None;
            }
            Some(norm)
        };
        let mut matched = Vec::new();
        let mut seen = BTreeSet::new();
        for token in tokens {
            if let Some(norm) = check_rel(token)
                && seen.insert(norm.clone())
            {
                matched.push(norm);
            }
        }
        if !matched.is_empty() {
            return Ok(matched);
        }
        let Some(re) = re_opt else {
            return Ok(Vec::new());
        };
        let mut visited = 0usize;
        let mut stack = vec![ws_canon.clone()];
        while let Some(dir) = stack.pop() {
            let mut entries = std::fs::read_dir(&dir)
                .map_err(|_| ())?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|_| ())?;
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                visited += 1;
                if visited > BINDING_WORKSPACE_SCAN_LIMIT {
                    return Err(());
                }
                let path = entry.path();
                let name = entry.file_name();
                let Some(name_str) = name.to_str() else {
                    continue;
                };
                if is_ignored_workspace_name_with(name_str, allowlist)
                    || name_str == ".harness-tmp"
                    || (dir == ws_canon && name_str == "target")
                {
                    continue;
                }
                let Ok(meta) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                if crate::custody::fs::linked(&meta) {
                    continue;
                }
                let Ok(canon) = crate::custody::fs::canonicalize(&path) else {
                    continue;
                };
                if !canon.starts_with(&ws_canon) {
                    continue;
                }
                let Ok(rel_path) = path.strip_prefix(&ws_canon) else {
                    continue;
                };
                let rel_raw = rel_path.to_string_lossy();
                let rel_slash = if cfg!(windows) {
                    rel_raw.replace('\\', "/")
                } else {
                    rel_raw.into_owned()
                };
                if let Some(norm) = normalize_safe_relative_path(&rel_slash) {
                    if re.is_match(&norm) && seen.insert(norm.clone()) {
                        matched.push(norm);
                    }
                    if meta.is_dir() {
                        stack.push(path);
                    }
                }
            }
        }
        Ok(matched)
    })
}

fn max_log_seq(path: &Path) -> u64 {
    state::read_log_at(path, 0)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|ev| ev["seq"].as_u64())
        .max()
        .unwrap_or(0)
}

fn run_state_is_paused(state_dir: &Path, package_dir: &Path) -> bool {
    let state_file = state_dir.join("plan-state.json");
    let Ok(Some(run_state)) = state::RunState::load(&state_file) else {
        return false;
    };
    let Ok(stored) = package::load(package_dir) else {
        return false;
    };
    run_state.capsule_sha256 == stored.capsule_sha256
        && run_state.position < stored.capsule.plan_len()
}

fn decide_for(
    runtime: &Runtime,
    session: &Value,
    route: &ResolvedCommand,
    message: &str,
    workspace: &Path,
    generation: Option<&str>,
) -> Decision {
    let session_state_root = session["id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|sid| runtime.store.state.join("skillcache/sessions").join(sid));
    let decision = decide_for_inner(runtime, session, route, message, workspace, generation);
    if generation.is_some()
        && let Decision::Miss { reason, .. } = &decision
        && *reason != "unavailable"
        && let Some(root) = &session_state_root
        && root.exists()
    {
        let _ = std::fs::remove_dir_all(root);
    }
    decision
}

fn decide_for_inner(
    runtime: &Runtime,
    session: &Value,
    route: &ResolvedCommand,
    message: &str,
    workspace: &Path,
    generation: Option<&str>,
) -> Decision {
    if !settings::warm(session) {
        return Decision::Off;
    }
    let arm = settings::eval_arm(session);
    if arm == Some("cold") {
        return Decision::Off;
    }
    let provider = session["provider"].as_str().unwrap_or("");
    let manual =
        crate::engine::state_store::session_permission_mode(session) == Some(crate::host::provider::PermissionMode::Manual);
    let bypass =
        crate::engine::state_store::session_permission_mode(session) == Some(crate::host::provider::PermissionMode::Bypass);
    let pin = settings::capsule_pin(session);
    let native_slash_skill = if route.mode == "native" && route.requests.is_empty() {
        match resolve_native_slash_skill(workspace, message) {
            Some(found) => Some(found),
            None => {
                return miss(
                    "native_route",
                    "a native command without a matching skill goes to the CLI as typed",
                );
            }
        }
    } else {
        None
    };
    let plan_only = session["mode"] != "edit";
    if !["claude", "codex", "cursor"].contains(&provider) {
        return miss(
            "provider",
            "warm turns are verified for Claude, Codex, and Cursor (textual) only",
        );
    }
    if provider == "cursor" && arm == Some("skillcache_executor") {
        return miss(
            "provider",
            "hybrid executor capsules run on Claude and Codex only",
        );
    }
    let codex_manual =
        provider == "codex" && manual && !matches!(arm, Some("raw_trace" | "written_procedure"));
    if arm == Some("raw_trace") {
        return decide_raw_trace(runtime, session, route, workspace);
    }
    let project_id = session["project_id"].as_str().unwrap_or("");
    let (chosen, skill_path) = if let Some(pinned_id) = pin {
        if route.requests.len() > 1 {
            return miss("no_single_skill", "the message names more than one skill");
        }
        let (requested_skill_path, requested_skill_name) = if let Some((name, path)) =
            native_slash_skill.as_ref()
        {
            (Some(path.clone()), name.as_str())
        } else if route.requests.len() == 1 {
            let path = match route.requests[0]["path"]
                .as_str()
                .and_then(|path| trace::spelled_skill(workspace, Path::new(path)))
            {
                Some(path) => path,
                None => {
                    return miss(
                        "no_capsule",
                        "the routed skill is not a skill folder of this project or of your home",
                    );
                }
            };
            let name = route.requests[0]["name"].as_str().unwrap_or("");
            (Some(path), name)
        } else {
            (None, "")
        };
        let requested_tree_sha = requested_skill_path
            .as_deref()
            .and_then(|path| trace::skill_file(workspace, path))
            .and_then(|file| file.parent().map(Path::to_path_buf))
            .and_then(|dir| crate::core::fingerprint::tree_sha256(&dir).ok())
            .map(|tree| tree.sha256)
            .unwrap_or_default();
        let row = match runtime
            .store
            .connection()
            .and_then(|db| db::capsule(&db, pinned_id))
        {
            Ok(Some(row)) => row,
            Ok(None) => return miss("no_capsule", "pinned capsule not found"),
            Err(error) => return miss("unavailable", error.to_string()),
        };
        if row["project_id"] != project_id
            || row["status"] != "active"
            || !capsule_matches_arm(&row, arm)
            || requested_skill_path.as_deref().is_some_and(|path| {
                !db::capsule_matches_skill_or_mirror(
                    &row,
                    path,
                    requested_skill_name,
                    &requested_tree_sha,
                )
            })
        {
            return miss(
                "no_capsule",
                "pinned capsule is not active for this project, skill, and evaluation arm",
            );
        }
        if provider == "cursor" && row["form"] != "textual" {
            return miss(
                "provider",
                "Cursor runs textual capsules only; hybrid capsules require Claude or Codex",
            );
        }
        if plan_only && !is_read_only_capsule(&row) {
            return miss(
                "plan_mode",
                "a capsule's programs write, and this is a read-only turn",
            );
        }
        let skill_path = row["skill_path"].as_str().unwrap_or("").to_string();
        (row, skill_path)
    } else if route.requests.is_empty()
        && native_slash_skill.is_none()
        && settings::tool_chain_enabled(&runtime.store, session)
    {
        match decide_toolchain_candidate(
            runtime, session, provider, arm, plan_only, workspace, message,
        ) {
            Ok(chosen) => chosen,
            Err(missed) => return missed,
        }
    } else {
        let (skill_name_owned, skill_path): (String, String) =
            if let Some((name, path)) = native_slash_skill.clone() {
                (name, path)
            } else {
                if route.requests.len() != 1 {
                    return miss(
                        "no_single_skill",
                        if route.requests.is_empty() {
                            "the message names no skill"
                        } else {
                            "the message names more than one skill"
                        },
                    );
                }
                let request = &route.requests[0];
                let Some(skill_path) = request["path"]
                    .as_str()
                    .and_then(|path| trace::spelled_skill(workspace, Path::new(path)))
                else {
                    return miss(
                        "no_capsule",
                        "the routed skill is not a skill folder of this project or of your home",
                    );
                };
                (
                    request["name"].as_str().unwrap_or("").to_owned(),
                    skill_path,
                )
            };
        let skill_name = skill_name_owned.as_str();
        let skill_tree_sha = trace::skill_file(workspace, &skill_path)
            .and_then(|file| file.parent().map(Path::to_path_buf))
            .and_then(|dir| crate::core::fingerprint::tree_sha256(&dir).ok())
            .map(|tree| tree.sha256)
            .unwrap_or_default();
        let common_dir = db::canonical_git_common_dir(workspace);
        let mut candidates = match runtime.store.connection().and_then(|db| {
            let siblings = db::sibling_worktree_projects(&db, project_id, workspace)?;
            match arm {
                None => db::active_capsules_for_skill_or_mirror_in_worktrees(
                    &db,
                    project_id,
                    &siblings,
                    common_dir.as_deref(),
                    &skill_path,
                    skill_name,
                    &skill_tree_sha,
                    None,
                ),
                Some("written_procedure" | "skillcache_no_replay") => {
                    db::active_capsules_for_skill_or_mirror_in_worktrees(
                        &db,
                        project_id,
                        &siblings,
                        common_dir.as_deref(),
                        &skill_path,
                        skill_name,
                        &skill_tree_sha,
                        arm,
                    )
                }
                Some("skillcache_agent" | "skillcache_executor") => {
                    let dedicated = db::active_capsules_for_skill_or_mirror_in_worktrees(
                        &db,
                        project_id,
                        &siblings,
                        common_dir.as_deref(),
                        &skill_path,
                        skill_name,
                        &skill_tree_sha,
                        arm,
                    )?;
                    Ok(if dedicated.is_empty() {
                        db::active_capsules_for_skill_or_mirror_in_worktrees(
                            &db,
                            project_id,
                            &siblings,
                            common_dir.as_deref(),
                            &skill_path,
                            skill_name,
                            &skill_tree_sha,
                            None,
                        )?
                    } else {
                        dedicated
                    })
                }
                Some(_) => Ok(Vec::new()),
            }
        }) {
            Ok(candidates) => candidates
                .into_iter()
                .filter(|cap| {
                    cap["project_id"] == project_id
                        || staleness_for_warm_candidate_in_project(
                            Some(&runtime.store.state),
                            Some(project_id),
                            cap,
                            workspace,
                            Some(&skill_path),
                        )
                        .is_none()
                })
                .collect::<Vec<_>>(),
            Err(error) => return miss("unavailable", error.to_string()),
        };
        if candidates.is_empty()
            && arm.is_none()
            && generation.is_some()
            && let Some(edition) = super::compile::workspace_edition_for_project(
                &runtime.store,
                project_id,
                workspace,
                Some(session),
            )
            && super::compile::admit_textual_templates_sync(
                runtime, project_id, workspace, &edition,
            )
            .is_ok_and(|admitted| !admitted.is_empty())
            && let Ok(refreshed) = runtime.store.connection().and_then(|db| {
                db::active_capsules_for_skill_or_mirror(
                    &db,
                    project_id,
                    &skill_path,
                    skill_name,
                    &skill_tree_sha,
                    None,
                )
            })
        {
            candidates = refreshed;
        }
        let has_any = !candidates.is_empty();
        let eligible = if provider == "cursor" {
            candidates
                .into_iter()
                .filter(|capsule| capsule["form"] == "textual")
                .collect::<Vec<_>>()
        } else {
            candidates
        };
        if eligible.is_empty() {
            if has_any && provider == "cursor" {
                return miss(
                    "provider",
                    "Cursor runs textual capsules only; no active textual capsule exists for this skill",
                );
            }
            return miss("no_capsule", "no qualified capsule exists for this skill");
        }
        let had_provider_eligible = !eligible.is_empty();
        let eligible = if plan_only {
            eligible
                .into_iter()
                .filter(is_read_only_capsule)
                .collect::<Vec<_>>()
        } else {
            eligible
        };
        let fresh: Vec<Value> = eligible
            .iter()
            .filter(|capsule| {
                capsule["project_id"] != project_id
                    || staleness_for_warm_candidate_in_project(
                        Some(&runtime.store.state),
                        Some(project_id),
                        capsule,
                        workspace,
                        (!skill_path.is_empty()).then_some(skill_path.as_str()),
                    )
                    .is_none()
            })
            .cloned()
            .collect();
        let base_pool = if fresh.is_empty() { &eligible } else { &fresh };
        let local_eligible: Vec<Value> = base_pool
            .iter()
            .filter(|capsule| capsule["project_id"] == project_id)
            .cloned()
            .collect();
        let ranked_eligible = if local_eligible.is_empty() {
            base_pool
        } else {
            &local_eligible
        };
        let chosen = match ranked_eligible.len() {
            0 if plan_only && had_provider_eligible => {
                return miss(
                    "plan_mode",
                    "a capsule's programs write, and this is a read-only turn",
                );
            }
            0 => return miss("no_capsule", "no qualified capsule exists for this skill"),
            1 => ranked_eligible[0].clone(),
            _ => {
                let by_description = runtime
                    .store
                    .setting(RETRIEVAL_BY_MU)
                    .ok()
                    .flatten()
                    .is_some_and(|value| value == true);
                let ids = ranked_eligible
                    .iter()
                    .filter_map(|capsule| capsule["capsule_id"].as_str().map(String::from))
                    .collect::<Vec<_>>();
                let best = by_description
                    .then(|| best_by_description(&runtime.store, &ids, message))
                    .flatten();
                match best.and_then(|id| {
                    ranked_eligible
                        .iter()
                        .find(|capsule| capsule["capsule_id"] == id.as_str())
                        .cloned()
                }) {
                    Some(capsule) => capsule,
                    None => best_by_health_and_recency(&runtime.store, ranked_eligible),
                }
            }
        };
        (chosen, skill_path)
    };
    let capsule_id = chosen["capsule_id"].as_str().unwrap_or("").to_string();
    let is_local_capsule = chosen["project_id"].as_str() == Some(project_id);
    if let Some(reason) = staleness_for_warm_candidate_in_project(
        Some(&runtime.store.state),
        Some(project_id),
        &chosen,
        workspace,
        (!skill_path.is_empty()).then_some(skill_path.as_str()),
    ) {
        if generation.is_some() && is_local_capsule {
            if let Ok(db) = runtime.store.connection() {
                let _ = db::set_capsule_status(&db, &capsule_id, "stale", Some(&reason));
            }
            runtime.publish(
                project_id,
                "stale",
                json!({"capsule_id": capsule_id, "reason": reason}),
            );
        }
        return miss("stale", reason);
    }
    let warm_dir = PathBuf::from(chosen["artifact_dir"].as_str().unwrap_or("")).join("warm");
    let stored = match package::load(&warm_dir) {
        Ok(stored) => stored,
        Err(error) => return miss("unavailable", error.to_string()),
    };
    // Codex outside Bypass runs its commands without the network.
    if stored.capsule.capabilities.network == Network::Required && provider == "codex" && !bypass {
        return miss(
            "network",
            "the capsule's programs need the network and Codex's sandbox has none",
        );
    }
    let name = stored.capsule.mu.name.clone();
    let skill_label = if skill_path.is_empty() {
        "workspace tool-chain"
    } else {
        skill_path.as_str()
    };
    let agent_only = arm == Some("skillcache_agent");
    let mut text = if agent_only {
        prompt::render_agent_driven(&stored, skill_label)
    } else {
        block(&stored, skill_label)
    };
    let engine = runtime.engine.clone();
    let codex = runtime.codex_executable();
    let eval_arm = arm.map(String::from);
    let all_read_only = stored.capsule.form == Form::Hybrid
        && !stored.capsule.programs.is_empty()
        && stored.capsule.programs.iter().all(|p| p.read_only);
    let all_program_steps = stored.capsule.form == Form::Hybrid
        && stored
            .capsule
            .plan()
            .all(|step| matches!(step, Step::Program { .. }));
    let explicit_direct = session["skillcache"]["direct_dispatch"].as_bool() == Some(true);
    let mode_allows_direct =
        plan_only || stored.capsule.kind != CapsuleKind::ToolChain || explicit_direct;
    let sid_opt = session["id"].as_str().filter(|s| !s.is_empty());
    let sid_key = sid_opt.unwrap_or("session");
    let state_subdir = if sid_opt.is_some() && !capsule_id.is_empty() {
        capsule_id.as_str()
    } else {
        generation.unwrap_or("preview")
    };
    let session_root = runtime
        .store
        .state
        .join("skillcache/sessions")
        .join(sid_key);
    let state_dir = session_root.join(state_subdir);

    // Inspect whether a prior multi-turn paused RunState applies to this capsule.
    let mut resolved_deterministic: Option<serde_json::Map<String, Value>> = None;
    let mut resume_note: Option<String> = None;
    let mut initial_seen = 0u64;
    let mut initial_seen_changes = 0u64;
    if stored.capsule.form == Form::Hybrid && sid_opt.is_some() {
        if generation.is_some()
            && session_root.exists()
            && let Ok(entries) = std::fs::read_dir(&session_root)
        {
            for entry in entries.flatten() {
                if entry.file_name().to_str() != Some(state_subdir) {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
        let state_file = state_dir.join("plan-state.json");
        if let Ok(Some(existing)) = state::RunState::load(&state_file) {
            let valid_pos = existing.capsule_sha256 == stored.capsule_sha256
                && existing.position < stored.capsule.plan_len();
            let candidate_bindings = if !stored.capsule.bindings.is_empty() {
                resolved_deterministic =
                    resolve_host_deterministic_bindings(&stored.capsule, route, message, workspace);
                resolved_deterministic.as_ref()
            } else {
                None
            };
            let bindings_conflict = candidate_bindings.is_some_and(|new_map| {
                new_map
                    .iter()
                    .any(|(k, v)| existing.bindings.get(k).is_some_and(|old| old != v))
            });
            if valid_pos && !bindings_conflict {
                initial_seen = max_log_seq(&state_dir.join(state::LOG_FILE));
                initial_seen_changes = max_log_seq(&state_dir.join(state::CHANGES_FILE));
                resume_note = Some(format!(
                    "\n\nPaused SkillCache run on disk: position {}/{} (awaiting {:?}). Call `skillcache-run status` or `skillcache-run continue` to resume it instead of restarting with `skillcache-run run`, or run `skillcache-run reset` before starting an unrelated request.",
                    existing.position + 1,
                    stored.capsule.plan_len(),
                    existing.awaiting,
                ));
            } else if generation.is_some() {
                let _ = std::fs::remove_dir_all(&state_dir);
            }
        }
    }
    let resuming = resume_note.is_some();
    if let Some(note) = &resume_note {
        text.push_str(note);
    }

    let can_try_direct = arm.is_none()
        && !agent_only
        && all_read_only
        && all_program_steps
        && !resuming
        && mode_allows_direct;
    let direct_bindings = if can_try_direct {
        if stored.capsule.bindings.is_empty() {
            Some(serde_json::Map::new())
        } else {
            resolved_deterministic.or_else(|| {
                resolve_host_deterministic_bindings(&stored.capsule, route, message, workspace)
            })
        }
    } else {
        None
    };
    let direct_dispatch = direct_bindings.is_some();
    if codex_manual && stored.capsule.form == Form::Hybrid && !direct_dispatch && !all_read_only {
        return miss(
            "codex_manual",
            "Codex in Manual mode would ask before every step of the executor",
        );
    }
    let (true, Some(generation)) = (stored.capsule.form == Form::Hybrid, generation) else {
        // A written procedure is read, and a preview stages nothing.
        return Decision::Hit(Arc::new(Warm {
            capsule_id,
            form: stored.capsule.form,
            eval_arm,
            name,
            block: text,
            environment: BTreeMap::new(),
            bin: None,
            package: None,
            state_dir: None,
            engine,
            codex,
            direct_dispatch,
            direct_bindings,
            seen: AtomicU64::new(0),
            seen_changes: AtomicU64::new(0),
        }));
    };
    let staged = (|| -> Result<(PathBuf, PathBuf)> {
        let bin = stage_tools_for(runtime, !agent_only)?;
        crate::custody::fs::ensure_directory(&state_dir, true)?;
        Ok((bin, state_dir))
    })();
    let (bin, state_dir) = match staged {
        Ok(staged) => staged,
        Err(error) => return miss("unavailable", error.to_string()),
    };
    let text_of = |path: &Path| path.display().to_string();
    let mut environment = BTreeMap::from([
        (env_filter::PACKAGE.to_string(), text_of(&warm_dir)),
        (env_filter::WORKSPACE.to_string(), text_of(workspace)),
        (env_filter::GENERATION.to_string(), generation.to_string()),
    ]);
    if agent_only {
        environment.insert(
            env_filter::LOG_FILE.to_string(),
            text_of(&state_dir.join("run-log.jsonl")),
        );
    } else {
        environment.insert(
            env_filter::STATE_FILE.to_string(),
            text_of(&state_dir.join("plan-state.json")),
        );
    }
    Decision::Hit(Arc::new(Warm {
        capsule_id,
        form: Form::Hybrid,
        eval_arm,
        name,
        block: text,
        environment,
        bin: Some(bin),
        package: Some(warm_dir),
        state_dir: Some(state_dir),
        engine,
        codex,
        direct_dispatch,
        direct_bindings,
        seen: AtomicU64::new(initial_seen),
        seen_changes: AtomicU64::new(initial_seen_changes),
    }))
}

/// Adds what a warm turn's provider process needs to the launch another
/// adapter prepared: the executor first on `PATH`, its variables, and leave
/// to call it without being asked.
pub struct WarmLaunchAdapter {
    pub inner: Arc<dyn LaunchAdapter>,
    pub warm: Arc<Warm>,
}
impl LaunchAdapter for WarmLaunchAdapter {
    fn adapt(&self, input: &Launch, command: PreparedCommand) -> Result<PreparedCommand> {
        let mut command = self.inner.adapt(input, command)?;
        let (Some(bin), Some(state_dir)) = (&self.warm.bin, &self.warm.state_dir) else {
            // A textual capsule is read; nothing is staged for it.
            return Ok(command);
        };
        let rest = env_filter::take_path(&mut command.environment)
            .or_else(|| std::env::var("PATH").ok())
            .unwrap_or_default();
        let path =
            std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(&rest)))
                .ok()
                .and_then(|joined| joined.into_string().ok())
                .ok_or_else(|| {
                    HarnessError::invalid("The executor's folder cannot be put on PATH.")
                })?;
        command.environment.insert("PATH".into(), path);
        command.environment.extend(self.warm.environment.clone());
        command.readable.push(bin.clone());
        if let Some(package) = &self.warm.package {
            command.readable.push(package.clone());
        }
        match input.provider.as_str() {
            "claude" => allow_claude(&mut command, !self.warm.agent_only())?,
            // Codex's sandbox writes only inside its workspace and the
            // folders it is given; the executor's state file is in neither.
            // Without the sandbox there is nothing to widen.
            "codex"
                if !command.argv.iter().any(|argument| {
                    argument == "app-server"
                        || argument == "--dangerously-bypass-approvals-and-sandbox"
                }) =>
            {
                let at = command
                    .argv
                    .iter()
                    .position(|argument| argument == "resume")
                    .unwrap_or_else(|| command.argv.len().saturating_sub(1));
                command.argv.splice(
                    at..at,
                    ["--add-dir".to_string(), state_dir.display().to_string()],
                );
            }
            _ => {}
        }
        Ok(command)
    }
}

/// Lets Claude Code run the SkillCache commands without asking.
fn allow_claude(command: &mut PreparedCommand, include_run: bool) -> Result<()> {
    let wrong = || HarnessError::invalid("Claude settings cannot take the SkillCache permission.");
    let index = command.argv.iter().position(|value| value == "--settings");
    let mut settings: Value = match index {
        Some(index) => serde_json::from_str(command.argv.get(index + 1).ok_or_else(wrong)?)?,
        None => json!({}),
    };
    let allow = settings
        .as_object_mut()
        .ok_or_else(wrong)?
        .entry("permissions")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(wrong)?
        .entry("allow")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(wrong)?;
    let rules: &[&str] = if include_run {
        &["Bash(skillcache-run:*)", "Bash(skillcache-tool:*)"]
    } else {
        &["Bash(skillcache-tool:*)"]
    };
    for &rule in rules {
        if !allow.iter().any(|known| known == rule) {
            allow.push(rule.into());
        }
    }
    match index {
        Some(index) => command.argv[index + 1] = settings.to_string(),
        None => command
            .argv
            .extend(["--settings".to_string(), settings.to_string()]),
    }
    Ok(())
}

fn phase_text(event: &Value) -> Option<(&'static str, String)> {
    let step = event["step_index"].as_u64().map(|index| index + 1);
    let program = event["program"].as_str().unwrap_or("");
    Some(match event["event"].as_str()? {
        "started" => (
            "started",
            format!(
                "SkillCache: the plan started ({} steps).",
                event["steps"].as_u64().unwrap_or_default()
            ),
        ),
        "needs_bindings" => (
            "needs_bindings",
            format!(
                "SkillCache: step {} needs {}.",
                step.unwrap_or_default(),
                event["names"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
        "your_steps" => (
            "your_steps",
            "SkillCache: the next steps are the agent's own.".to_string(),
        ),
        "program_completed" => (
            "program_completed",
            format!(
                "SkillCache: program `{program}` ran (step {}).",
                step.unwrap_or_default()
            ),
        ),
        "program_failed" => (
            "program_failed",
            format!(
                "SkillCache: program `{program}` failed ({}); step {} is the agent's now, the rest of the plan stays compiled.",
                event["reason"].as_str().unwrap_or("failed"),
                step.unwrap_or_default()
            ),
        ),
        "interrupted" => (
            "interrupted",
            format!(
                "SkillCache: step {} was interrupted and is resumed.",
                step.unwrap_or_default()
            ),
        ),
        "complete" => (
            "complete",
            format!(
                "SkillCache: the plan is complete ({} failed).",
                event["failed"].as_u64().unwrap_or_default()
            ),
        ),
        _ => return None,
    })
}

impl Warm {
    fn log_file(&self) -> Option<PathBuf> {
        let dir = self.state_dir.as_ref()?;
        if self.agent_only() {
            Some(dir.join("run-log.jsonl"))
        } else {
            Some(state::log_path(&dir.join("plan-state.json")))
        }
    }
    /// Turns the executor's new journal lines into session events. A line is
    /// published once: the journal numbers them and the last one seen is kept.
    pub fn ingest(&self, emit: &dyn Fn(Value) -> Result<()>) {
        self.ingest_with_changes(emit, &|_| {});
    }

    /// Like [`Self::ingest`], also forwarding workspace `changed_files` mutations
    /// from the sibling `run-log.changes.jsonl` sidecar to `on_changes` as
    /// normalized `file_change` tool events for live focus tracking.
    pub fn ingest_with_changes(
        &self,
        emit: &dyn Fn(Value) -> Result<()>,
        on_changes: &dyn Fn(Value),
    ) {
        let Some(log_file) = self.log_file() else {
            return;
        };
        let after = self.seen.load(Ordering::Acquire);
        if let Ok(events) = state::read_log_at(&log_file, after) {
            for event in events {
                let Some(seq) = event["seq"].as_u64() else {
                    continue;
                };
                // Another caller may have published this line meanwhile.
                if self.seen.fetch_max(seq, Ordering::AcqRel) >= seq {
                    continue;
                }
                if let Some((phase, text)) = phase_text(&event) {
                    let _ = emit(json!({
                        "kind": "skillcache",
                        "phase": phase,
                        "capsule_id": self.capsule_id,
                        "seq": seq,
                        "step_index": event["step_index"],
                        "program": event["program"],
                        "reason": event["reason"],
                        "verification": event["verification"],
                        "text": text,
                    }));
                }
            }
        }
        let after_changes = self.seen_changes.load(Ordering::Acquire);
        if let Ok(change_events) =
            state::read_log_at(&state::changes_path(&log_file), after_changes)
        {
            for event in change_events {
                let Some(seq) = event["seq"].as_u64() else {
                    continue;
                };
                if self.seen_changes.fetch_max(seq, Ordering::AcqRel) >= seq {
                    continue;
                }
                if let Some(changes) = event["changes"].as_array().filter(|arr| !arr.is_empty()) {
                    on_changes(json!({
                        "kind": "tool",
                        "state": "completed",
                        "tool": "file_change",
                        "ok": true,
                        "changes": changes,
                    }));
                }
            }
        }
    }
    /// Records how the warm turn went and removes its working state.
    pub fn finish(
        &self,
        store: &SessionStore,
        session: &Value,
        generation: &str,
        emit: &dyn Fn(Value) -> Result<()>,
    ) {
        self.ingest(emit);
        let events = self
            .log_file()
            .and_then(|log_file| state::read_log_at(&log_file, 0).ok())
            .unwrap_or_default();
        let count = |name: &str| events.iter().filter(|event| event["event"] == name).count();
        let outcome = if self.form == Form::Textual {
            "read"
        } else if self.agent_only() {
            // The agent drives the programs itself, so nothing says "the plan
            // ended": it is complete when every program step of it ran to its
            // end and none failed, and unfinished when only some did.
            let planned = self
                .package
                .as_deref()
                .and_then(|dir| package::load(dir).ok())
                .map(|stored| {
                    stored
                        .capsule
                        .plan()
                        .filter(|step| step.program().is_some())
                        .count()
                });
            let completed = events
                .iter()
                .filter(|event| event["event"] == "program_completed")
                .filter_map(|event| event["step_index"].as_u64())
                .collect::<std::collections::BTreeSet<_>>()
                .len();
            let failed = count("program_failed");
            if failed == 0 && planned.is_some_and(|planned| planned > 0 && completed >= planned) {
                "complete"
            } else if completed > 0 || failed > 0 {
                "unfinished"
            } else {
                "not_started"
            }
        } else if count("complete") > 0 {
            "complete"
        } else if count("started") > 0 {
            "unfinished"
        } else {
            "not_started"
        };
        let steps = events
            .iter()
            .map(|event| {
                json!({
                    "seq": event["seq"], "event": event["event"], "step_index": event["step_index"],
                    "program": event["program"], "reason": event["reason"], "at_ms": event["at_ms"],
                })
            })
            .collect::<Vec<_>>();
        let deopts = count("program_failed") as i64;
        // Read before the connection is taken: the store has one, and both a
        // setting and a session event go through it.
        let thresholds = recompile_thresholds(store);
        let mut waits: Option<(String, Option<String>)> = None;
        if !self.capsule_id.is_empty()
            && let Ok(db) = store.connection()
        {
            let _ = db::record_run(
                &db,
                &db::Run {
                    generation,
                    session_id: session["id"].as_str().unwrap_or(""),
                    capsule_id: &self.capsule_id,
                    provider: session["provider"].as_str().unwrap_or(""),
                    model: session["model"].as_str(),
                    outcome,
                    steps: &Value::Array(steps),
                    deopts,
                },
            );
            let _ = db::touch_capsule(&db, &self.capsule_id);
            // Behind its setting only: enough failing warm turns since the
            // last qualification hand back to a person.
            if (deopts > 0 || matches!(outcome, "unfinished" | "not_started"))
                && let Some((window, failed)) = thresholds
                && let Ok(Some(cap)) = db::capsule(&db, &self.capsule_id)
                && let Some(since) = cap["updated_at"].as_str()
                && let Ok(recent) = db::capsule_runs_since(&db, &self.capsule_id, since, window)
            {
                let handed_back = recent
                    .iter()
                    .filter(|run| {
                        run["deopts"].as_i64().unwrap_or_default() > 0
                            || matches!(run["outcome"].as_str(), Some("unfinished" | "not_started"))
                    })
                    .count();
                let reason = format!(
                    "{handed_back} of its last {} warm turns handed a program step back.",
                    recent.len()
                );
                if handed_back >= failed
                    && db::set_capsule_status(
                        &db,
                        &self.capsule_id,
                        "needs_recompile",
                        Some(&reason),
                    )
                    .is_ok()
                {
                    let _ = db::mark_capsule_needs_trace(&db, &self.capsule_id);
                    let project_id = cap["project_id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .map(String::from);
                    waits = Some((reason, project_id));
                }
            }
        }
        if let Some((reason, project_id)) = waits {
            store.publish_event(
                "skillcache_capsule",
                None,
                project_id.clone(),
                json!({
                    "state": "needs_recompile",
                    "project_id": project_id,
                    "capsule_id": self.capsule_id,
                    "reason": reason,
                    "needs_trace": true,
                }),
            );
            let _ = emit(json!({
                "kind": "skillcache",
                "phase": "needs_recompile",
                "capsule_id": self.capsule_id,
                "text": format!("SkillCache: {reason} The capsule is not used until it is requalified or compiled again."),
            }));
        }
        let keep_paused = self
            .state_dir
            .as_deref()
            .zip(self.package.as_deref())
            .is_some_and(|(sd, pkg)| run_state_is_paused(sd, pkg));
        if !keep_paused && let Some(state_dir) = &self.state_dir {
            let _ = std::fs::remove_dir_all(state_dir);
        }
    }

    /// Removes temporary state when a warm turn failed or was cancelled before finishing,
    /// unless a mid-plan run state (`position < plan_len` after step 0) is paused cleanly.
    pub(crate) fn cleanup_abandoned(&self) {
        let keep_paused = self
            .state_dir
            .as_deref()
            .zip(self.package.as_deref())
            .is_some_and(|(sd, pkg)| {
                run_state_is_paused(sd, pkg)
                    && state::RunState::load(&sd.join("plan-state.json"))
                        .ok()
                        .flatten()
                        .is_some_and(|rs| rs.position > 0)
            });
        if !keep_paused && let Some(state_dir) = &self.state_dir {
            let _ = std::fs::remove_dir_all(state_dir);
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PartialDirectFailure {
    pub step_index: usize,
    pub program: String,
    pub instruction: String,
    pub diagnostics: Value,
    pub required_outputs: Vec<String>,
    pub prefix_results: Value,
}

pub(crate) enum DirectDispatchOutcome {
    Completed {
        answer: Value,
        formatted: String,
    },
    Fallback {
        reason: String,
        program_failed: bool,
        partial: Option<PartialDirectFailure>,
    },
}

/// Formats verified deterministic program outputs concisely for direct user return
/// or light prose wrapping.
pub fn format_direct_output(capsule_name: &str, answer: &Value) -> String {
    let results = answer
        .get("result")
        .filter(|v| v.is_object())
        .or_else(|| answer.get("results").filter(|v| v.is_object()))
        .unwrap_or(&Value::Null);
    if let Some(obj) = results.as_object() {
        const PROSE_KEYS: [&str; 6] =
            ["markdown", "summary", "report", "output", "text", "message"];
        for key in PROSE_KEYS {
            if let Some(s) = obj
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return s.to_owned();
            }
        }
        if obj.len() == 1
            && let Some((_, single)) = obj.iter().next()
        {
            for key in PROSE_KEYS {
                if let Some(s) = single
                    .get(key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    return s.to_owned();
                }
            }
        }
        let formatted_json = serde_json::to_string_pretty(results).unwrap_or_else(|_| "{}".into());
        return format!("Verified output ({capsule_name}):\n```json\n{formatted_json}\n```");
    }
    serde_json::to_string_pretty(answer).unwrap_or_else(|_| answer.to_string())
}

struct DirectSandboxDir(PathBuf);
impl Drop for DirectSandboxDir {
    fn drop(&mut self) {
        package::release(&self.0);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn push_unique_path(list: &mut Vec<PathBuf>, path: PathBuf) {
    if !list.contains(&path) {
        list.push(path);
    }
}

struct DirectSandboxInputs<'a> {
    engine: &'a super::gates::Engine,
    package_dir: &'a Path,
    workspace: &'a Path,
    state_dir: &'a Path,
    sandbox_ws: &'a Path,
    protected: &'a [PathBuf],
    stored: &'a package::Package,
    all_read_only: bool,
}

fn build_direct_sandbox_spec(
    inputs: DirectSandboxInputs<'_>,
) -> std::result::Result<(String, PathBuf, PathBuf, PathBuf, Spec), String> {
    let canon = |path: &Path| {
        crate::custody::fs::canonicalize(path).map_err(|err| format!("{}: {err}", path.display()))
    };
    let canon_pkg = canon(inputs.package_dir)?;
    let canon_ws = canon(inputs.workspace)?;
    let canon_state = canon(inputs.state_dir)?;
    let canon_sandbox_ws = canon(inputs.sandbox_ws)?;
    let mut canon_protected = Vec::with_capacity(inputs.protected.len());
    for path in inputs.protected {
        canon_protected.push(canon(path)?);
    }
    let (executable, extra_readable) = inputs
        .engine
        .command(&canon_sandbox_ws)
        .map_err(|err| err.to_string())?;
    let mut readable = Vec::new();
    for path in extra_readable {
        push_unique_path(&mut readable, canon(&path)?);
    }
    push_unique_path(&mut readable, canon_pkg.clone());
    let mut writable = vec![canon_state.clone()];
    if inputs.all_read_only {
        push_unique_path(&mut readable, canon_ws.clone());
    } else {
        push_unique_path(&mut writable, canon_ws.clone());
    }
    let spec = Spec {
        workspace: canon_sandbox_ws.clone(),
        target: canon_sandbox_ws,
        protected: canon_protected,
        readable,
        accounts: writable,
        runtime: super::gates::gate_runtime_dirs(&inputs.stored.capsule.capabilities.runtimes),
        codex: inputs.engine.codex.clone(),
        evidence: None,
        network: inputs.stored.capsule.capabilities.network == Network::Required,
    };
    Ok((executable, canon_pkg, canon_ws, canon_state, spec))
}

/// Executes an eligible deterministic read-only capsule directly inside
/// `crate::host::sandbox::gate`, avoiding the ~25k prefix LLM loop while falling
/// back cleanly on sandbox refusal, error, missing bindings, residual steps, or program failure.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn try_direct_execute(
    warm: &Warm,
    workspace: &Path,
    generation: &str,
    supplied_bindings: Option<&Value>,
) -> DirectDispatchOutcome {
    try_direct_execute_with(warm, workspace, generation, supplied_bindings, &[], None)
}

fn direct_fallback(reason: impl Into<String>, program_failed: bool) -> DirectDispatchOutcome {
    DirectDispatchOutcome::Fallback {
        reason: reason.into(),
        program_failed,
        partial: None,
    }
}

fn partial_direct_failure(state_dir: &Path, answer: &Value) -> Option<PartialDirectFailure> {
    let step_index = answer["step_index"]
        .as_u64()
        .and_then(|v| usize::try_from(v).ok())?;
    if step_index == 0 {
        return None;
    }
    let state_file = state_dir.join("plan-state.json");
    let run_state = state::RunState::load(&state_file).ok().flatten()?;
    if !matches!(
        run_state.awaiting,
        state::Awaiting::Outputs { step_index: s, .. } if s == step_index
    ) {
        return None;
    }
    let required_outputs = answer["required_outputs"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let prefix_results = if answer["results"].is_object() {
        answer["results"].clone()
    } else {
        Value::Object(run_state.results)
    };
    Some(PartialDirectFailure {
        step_index,
        program: answer["program"].as_str().unwrap_or("").to_owned(),
        instruction: answer["instruction"].as_str().unwrap_or("").to_owned(),
        diagnostics: answer["diagnostics"].clone(),
        required_outputs,
        prefix_results,
    })
}

/// Executes an eligible deterministic read-only capsule inside `crate::host::sandbox::gate`,
/// passing optional protected boundary paths and cancellation signal.
pub(crate) fn try_direct_execute_with(
    warm: &Warm,
    workspace: &Path,
    generation: &str,
    supplied_bindings: Option<&Value>,
    protected: &[PathBuf],
    cancel: Option<&Arc<AtomicBool>>,
) -> DirectDispatchOutcome {
    if !warm.direct_dispatch {
        return direct_fallback("not_eligible", false);
    }
    let (Some(package_dir), Some(state_dir)) = (&warm.package, &warm.state_dir) else {
        return direct_fallback("missing_stage", false);
    };
    let stored = match package::load(package_dir) {
        Ok(pkg) => pkg,
        Err(err) => return direct_fallback(err.to_string(), false),
    };
    let all_read_only = stored.capsule.programs.iter().all(|p| p.read_only);
    let all_program_steps = stored
        .capsule
        .plan()
        .all(|step| matches!(step, Step::Program { .. }));
    if !all_read_only || !all_program_steps {
        return direct_fallback("requires_agent", false);
    }
    let effective_bindings = supplied_bindings.cloned().or_else(|| {
        warm.direct_bindings
            .as_ref()
            .filter(|m| !m.is_empty())
            .map(|m| Value::Object(m.clone()))
    });
    if !stored.capsule.bindings.is_empty() && effective_bindings.is_none() {
        return direct_fallback("needs_bindings", false);
    }
    let default_cancel = Arc::new(AtomicBool::new(false));
    let cancel_flag = cancel.unwrap_or(&default_cancel);
    if cancel_flag.load(Ordering::Acquire) {
        return direct_fallback("cancelled", false);
    }
    let sandbox_base = state_dir.parent().unwrap_or(state_dir);
    let sandbox_root = sandbox_base.join(format!("direct-sandbox-{}", crate::engine::state_store::store_helpers::hexid()));
    let _sandbox_cleanup = DirectSandboxDir(sandbox_root.clone());
    let sandbox_ws = sandbox_root.join("work");
    let scratch = sandbox_ws.join(".harness-tmp");
    if let Err(err) = crate::custody::fs::ensure_directory(&scratch, true)
        .and_then(|_| crate::custody::fs::ensure_directory(state_dir, true))
    {
        return direct_fallback(format!("sandbox_unavailable: {err}"), false);
    }
    let engine = super::gates::Engine {
        path: warm.engine.clone(),
        codex: warm.codex.clone(),
    };
    let (executable, canon_pkg, canon_ws, canon_state, spec) =
        match build_direct_sandbox_spec(DirectSandboxInputs {
            engine: &engine,
            package_dir,
            workspace,
            state_dir,
            sandbox_ws: &sandbox_ws,
            protected,
            stored: &stored,
            all_read_only,
        }) {
            Ok(parts) => parts,
            Err(err) => return direct_fallback(format!("sandbox_unavailable: {err}"), false),
        };
    let canon_scratch = match crate::custody::fs::canonicalize(&scratch) {
        Ok(path) => path,
        Err(err) => return direct_fallback(format!("sandbox_unavailable: {err}"), false),
    };
    let state_file = canon_state.join("plan-state.json");
    let environment = super::gates::sandbox_environment(
        &engine,
        &canon_pkg,
        &canon_ws,
        &canon_scratch,
        Some(&state_file),
        generation,
    );
    let (argv, stdin_bytes) = if let Some(bindings) = &effective_bindings {
        match serde_json::to_vec(bindings) {
            Ok(bytes) => (
                vec![executable, "skillcache".into(), "run".into(), "-".into()],
                bytes,
            ),
            Err(err) => return direct_fallback(format!("invalid_bindings: {err}"), false),
        }
    } else {
        (
            vec![executable, "skillcache".into(), "run".into()],
            Vec::new(),
        )
    };
    let seconds = stored
        .capsule
        .programs
        .iter()
        .map(|p| p.timeout_seconds)
        .sum::<u64>()
        .saturating_add(15);
    let ran = match super::gates::run_sandboxed_with_input(
        &spec,
        &argv,
        &environment,
        seconds,
        &stdin_bytes,
        cancel_flag,
    ) {
        Ok(ran) => ran,
        Err(err) => return direct_fallback(format!("sandbox_unavailable: {err}"), false),
    };
    if let Some(stopped) = ran.stopped {
        return direct_fallback(stopped, true);
    }
    let parsed = crate::custody::strict_json::parse(
        String::from_utf8_lossy(&ran.stdout).trim().as_bytes(),
        8 * 1024 * 1024,
        32,
        200_000,
    );
    if ran.code != Some(0) {
        if let Ok(answer) = parsed {
            let code = answer["code"].as_str().unwrap_or("");
            let status = answer["status"].as_str().unwrap_or("");
            let program_failed = code == "program_failed" || status == "program_failed";
            let reason = answer["message"]
                .as_str()
                .map(str::to_owned)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| super::gates::detail(&ran.stderr));
            let partial = if program_failed {
                partial_direct_failure(&canon_state, &answer)
            } else {
                None
            };
            return DirectDispatchOutcome::Fallback {
                reason,
                program_failed,
                partial,
            };
        }
        let code_desc = ran
            .code
            .map_or_else(|| "signal".to_owned(), |c| format!("exit_{c}"));
        let stderr_detail = super::gates::detail(&ran.stderr);
        let reason = if stderr_detail.is_empty() {
            format!("sandbox_{code_desc}")
        } else {
            format!("sandbox_{code_desc}: {stderr_detail}")
        };
        return direct_fallback(reason, false);
    }
    match parsed {
        Ok(answer) => {
            let status = answer["status"].as_str().unwrap_or("");
            let no_failures = answer["failed"].as_array().is_none_or(Vec::is_empty);
            if status == "complete" && no_failures {
                let formatted = format_direct_output(&warm.name, &answer);
                return DirectDispatchOutcome::Completed { answer, formatted };
            }
            let program_failed =
                status == "program_failed" || answer["code"] == "program_failed" || !no_failures;
            let reason = if status == "program_failed" && !answer["diagnostics"].is_null() {
                format!("program_failed: {}", answer["diagnostics"])
            } else if let Some(msg) = answer["message"].as_str().filter(|s| !s.is_empty()) {
                msg.to_owned()
            } else {
                status.to_owned()
            };
            let partial = if program_failed {
                partial_direct_failure(&canon_state, &answer)
            } else {
                None
            };
            DirectDispatchOutcome::Fallback {
                reason,
                program_failed,
                partial,
            }
        }
        Err(_) => {
            let stderr_detail = super::gates::detail(&ran.stderr);
            direct_fallback(
                if stderr_detail.is_empty() {
                    "invalid_sandbox_output".to_owned()
                } else {
                    format!("invalid_sandbox_output: {stderr_detail}")
                },
                false,
            )
        }
    }
}

