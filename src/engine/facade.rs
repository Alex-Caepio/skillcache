//! High-level standalone facade unifying SessionStore, traces, compiler, Gate 1/2/3, and warm status.
use crate::core::{
    Form, SkillCapsule,
    candidate::{Candidate, KnownSkill},
    capsule::Examples,
    gate1, package,
    trace::{Recorder, Redactor, Trace},
};
use crate::custody;
use crate::engine::{
    SkillCacheDomain, compile, db, replay_result, snapshot,
    state_store::{ProviderDomain, ResolvedCommand, SessionStore, store_helpers as store},
    trace,
};
use crate::error::{HarnessError, Result};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};

pub fn default_state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SKILLCACHE_STATE_DIR").filter(|s| !s.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(xdg).join("skillcache");
    }
    if let Some(home) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(home).join(".local/state/skillcache");
    }
    std::env::temp_dir().join("skillcache-state")
}

#[derive(Clone, Debug, Default)]
pub struct CompileOptions {
    pub project: PathBuf,
    pub generation: Option<String>,
    pub skill: Option<String>,
    pub trace_path: Option<PathBuf>,
    pub candidate_path: Option<PathBuf>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub round_dry_run: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ListCapsulesOptions {
    pub project: PathBuf,
    pub skill: Option<String>,
    pub status: Option<String>,
    pub form: Option<String>,
    pub include_retired: bool,
}

#[derive(Clone, Debug, Default)]
pub struct TracesOptions {
    pub project: PathBuf,
    pub action: String,
    pub generation: Option<String>,
    pub trace_path: Option<PathBuf>,
    pub skill: Option<String>,
}

pub struct SkillCacheEngine {
    pub store: SessionStore,
    pub providers: Arc<ProviderDomain>,
    pub domain: SkillCacheDomain,
    pub runtime: Arc<compile::Runtime>,
}

impl SkillCacheEngine {
    pub fn open(state_dir: Option<&Path>) -> Result<Self> {
        let state = state_dir
            .map(PathBuf::from)
            .unwrap_or_else(default_state_dir);
        let store = SessionStore::open(&state)?;
        let providers = Arc::new(ProviderDomain::discover_sync());
        let cancel = Arc::new(AtomicBool::new(false));
        let domain = SkillCacheDomain::new(store.clone(), providers.clone(), cancel);
        let runtime = domain.runtime();
        Ok(Self {
            store,
            providers,
            domain,
            runtime,
        })
    }

    pub fn open_with_providers(
        state_dir: Option<&Path>,
        providers: Arc<ProviderDomain>,
        engine_path: Option<PathBuf>,
    ) -> Result<Self> {
        let state = state_dir
            .map(PathBuf::from)
            .unwrap_or_else(default_state_dir);
        let store = SessionStore::open(&state)?;
        let cancel = Arc::new(AtomicBool::new(false));
        let mut domain = SkillCacheDomain::new(store.clone(), providers.clone(), cancel);
        if let Some(engine) = engine_path {
            domain = domain.with_engine(engine);
        }
        let runtime = domain.runtime();
        Ok(Self {
            store,
            providers,
            domain,
            runtime,
        })
    }

    pub fn status(&self, project: &Path) -> Result<Value> {
        let project_rec = self.store.register(project)?;
        let project_id = project_rec["id"]
            .as_str()
            .ok_or_else(|| HarnessError::unavailable("Project registration returned no id."))?;
        let listed = self
            .domain
            .list(project_id, &json!({"include_retired": true}))?;
        let (counts, stale_reasons) =
            summarize_capsules(listed["capsules"].as_array().map(Vec::as_slice).unwrap_or(&[]));
        let db = self.store.connection()?;
        let trace_count: i64 = db.query_row(
            "SELECT COUNT(*) FROM skillcache_traces t
             LEFT JOIN sessions s ON s.id = t.session_id
             WHERE s.project_id = ?1 OR t.session_id = ?2",
            rusqlite::params![project_id, project_id],
            |row| row.get(0),
        )?;
        let pending_requalify: i64 = db.query_row(
            "SELECT COUNT(*) FROM skillcache_pending_requalify WHERE project_id = ?1",
            rusqlite::params![project_id],
            |row| row.get(0),
        )?;
        Ok(json!({
            "project_id": project_id,
            "project": project_rec["path"],
            "edition": project_rec["edition"],
            "state_dir": self.store.state.display().to_string(),
            "sandbox_available": crate::host::sandbox::available(),
            "providers": self.providers.listing()?,
            "traces": trace_count,
            "pending_requalify": pending_requalify,
            "capsule_counts": counts,
            "stale_reasons": stale_reasons,
        }))
    }

    pub fn capsules_list(&self, opts: &ListCapsulesOptions) -> Result<Value> {
        let project_rec = self.store.register(&opts.project)?;
        let project_id = project_rec["id"]
            .as_str()
            .ok_or_else(|| HarnessError::unavailable("Project registration returned no id."))?;
        let query = json!({
            "skill": opts.skill,
            "status": opts.status,
            "form": opts.form,
            "include_retired": opts.include_retired,
        });
        let mut res = self.domain.list(project_id, &query)?;
        let (counts, stale_reasons) =
            summarize_capsules(res["capsules"].as_array().map(Vec::as_slice).unwrap_or(&[]));
        if let Some(obj) = res.as_object_mut() {
            obj.insert("project".into(), project_rec["path"].clone());
            obj.insert("counts".into(), counts);
            obj.insert("stale_reasons".into(), stale_reasons);
        }
        Ok(res)
    }

    pub fn capsule_inspect(&self, project: &Path, capsule_id: &str) -> Result<Value> {
        let project_rec = self.store.register(project)?;
        let project_id = project_rec["id"]
            .as_str()
            .ok_or_else(|| HarnessError::unavailable("Project registration returned no id."))?;
        let mut detail = self.domain.get(project_id, capsule_id)?;
        if let Some(obj) = detail.as_object_mut() {
            obj.insert("project".into(), project_rec["path"].clone());
        }
        Ok(detail)
    }

    pub fn gate1(package_dir: &Path, examples_path: Option<&Path>) -> Result<Value> {
        let abs = custody::fs::canonicalize(package_dir)?;
        if custody::inside_plugin_dir(&abs) {
            return Err(HarnessError::coded(
                crate::error::ErrorKind::Policy,
                "plugin-cwd",
                "Refusing Gate 1 against files inside the skillcache plugin directory.",
            ));
        }
        let pkg = package::load(&abs)?;
        let capsule_bytes = custody::fs::read_regular(&abs.join(package::CAPSULE_FILE))?;
        let examples_bytes = examples_path
            .map(custody::fs::read_regular)
            .transpose()?;
        let report = match pkg.markdown.as_deref() {
            Some(markdown) => {
                let parsed_examples = examples_bytes
                    .as_deref()
                    .map(Examples::parse)
                    .transpose()
                    .map_err(|_| HarnessError::invalid("Invalid examples JSON file."))?;
                gate1::check_textual(&capsule_bytes, markdown.as_bytes(), parsed_examples.as_ref())
            }
            None => gate1::check(
                &capsule_bytes,
                &pkg.files,
                examples_bytes.as_deref(),
                &gate1::Context::default(),
            ),
        };
        Ok(serde_json::to_value(&report)?)
    }

    pub async fn requalify(&self, project: &Path, capsule_id: &str) -> Result<Value> {
        let project_rec = self.store.register(project)?;
        let project_id = project_rec["id"]
            .as_str()
            .ok_or_else(|| HarnessError::unavailable("Project registration returned no id."))?;
        let workspace = PathBuf::from(
            project_rec["path"]
                .as_str()
                .ok_or_else(|| HarnessError::unavailable("Missing registered project path."))?,
        );
        let capsule = self.domain.owned(project_id, capsule_id)?;
        let status = capsule["status"].as_str().unwrap_or("");
        if !matches!(status, "active" | "stale" | "needs_recompile" | "retired") {
            return Err(HarnessError::conflict(
                "Only a compiled capsule can be requalified.",
            ));
        }
        let verdict = compile::requalify(&self.runtime, &capsule, &workspace).await?;
        let passed = verdict["passed"] == true;
        let db = self.store.connection()?;
        if passed {
            db::set_capsule_status(&db, capsule_id, "active", None)?;
            let _ = db::delete_pending_requalify(&db, capsule_id);
        } else if status == "retired" {
            db::set_capsule_status(
                &db,
                capsule_id,
                "retired",
                Some("Requalification failed."),
            )?;
        } else {
            db::set_capsule_status(
                &db,
                capsule_id,
                "stale",
                Some("Requalification failed; inspect gate reports."),
            )?;
        }
        drop(db);
        let updated = self.domain.get(project_id, capsule_id)?;
        Ok(json!({
            "capsule_id": capsule_id,
            "project_id": project_id,
            "passed": passed,
            "verdict": verdict,
            "capsule": updated,
        }))
    }

    pub fn retire(&self, project: &Path, capsule_id: &str, reason: Option<&str>) -> Result<Value> {
        let project_rec = self.store.register(project)?;
        let project_id = project_rec["id"]
            .as_str()
            .ok_or_else(|| HarnessError::unavailable("Project registration returned no id."))?;
        let body = match reason.map(str::trim).filter(|r| !r.is_empty()) {
            Some(r) => json!({"reason": r}),
            None => Value::Null,
        };
        self.domain.retire(project_id, capsule_id, &body)
    }

    pub fn invalidate(&self, project: &Path, capsule_id: &str) -> Result<Value> {
        let project_rec = self.store.register(project)?;
        let project_id = project_rec["id"]
            .as_str()
            .ok_or_else(|| HarnessError::unavailable("Project registration returned no id."))?;
        self.domain.invalidate(project_id, capsule_id)
    }

    pub fn traces(&self, opts: &TracesOptions) -> Result<Value> {
        let project_rec = self.store.register(&opts.project)?;
        let project_id = project_rec["id"]
            .as_str()
            .ok_or_else(|| HarnessError::unavailable("Project registration returned no id."))?;
        let workspace = PathBuf::from(
            project_rec["path"]
                .as_str()
                .ok_or_else(|| HarnessError::unavailable("Missing registered project path."))?,
        );
        let action = if opts.action.trim().is_empty() {
            "list"
        } else {
            opts.action.trim()
        };
        match action {
            "list" => {
                let db = self.store.connection()?;
                let mut stmt = db.prepare(
                    "SELECT t.generation, t.session_id, t.provider, t.model, t.request, t.skill_refs, t.steps, t.usage, t.redaction, t.trace_sha256, t.created_at
                     FROM skillcache_traces t
                     LEFT JOIN sessions s ON s.id = t.session_id
                     WHERE s.project_id = ?1 OR t.session_id = ?1
                     ORDER BY t.created_at DESC, t.rowid DESC",
                )?;
                let rows = stmt.query_map(rusqlite::params![project_id], |row| {
                    let skill_refs_raw: String = row.get(5)?;
                    let steps_raw: String = row.get(6)?;
                    let usage_raw: Option<String> = row.get(7)?;
                    let redaction_raw: String = row.get(8)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                        serde_json::from_str::<Value>(&skill_refs_raw).unwrap_or_else(|_| json!([])),
                        serde_json::from_str::<Value>(&steps_raw).unwrap_or_else(|_| json!([])),
                        usage_raw
                            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                            .unwrap_or(Value::Null),
                        serde_json::from_str::<Value>(&redaction_raw).unwrap_or_else(|_| json!({})),
                        row.get::<_, String>(9)?,
                        row.get::<_, String>(10)?,
                    ))
                })?;
                let mut traces = Vec::new();
                for item in rows {
                    let (
                        generation,
                        session_id,
                        provider,
                        model,
                        request,
                        skill_refs,
                        steps,
                        usage,
                        redaction,
                        trace_sha256,
                        created_at,
                    ) = item?;
                    if let Some(filter_skill) = opts.skill.as_deref().filter(|s| !s.is_empty()) {
                        let matches_skill = skill_refs.as_array().into_iter().flatten().any(|s| {
                            s["name"].as_str() == Some(filter_skill)
                                || s["path"].as_str() == Some(filter_skill)
                        });
                        if !matches_skill {
                            continue;
                        }
                    }
                    let has_snap = snapshot::load(&self.store.state, &generation)
                        .is_ok_and(|s| s.is_some());
                    let has_replay = replay_result::available(&self.store.state, &generation);
                    traces.push(json!({
                        "generation": generation,
                        "session_id": session_id,
                        "provider": provider,
                        "model": model,
                        "request": request,
                        "skill_refs": skill_refs,
                        "steps_count": steps.as_array().map_or(0, Vec::len),
                        "usage": usage,
                        "redaction": redaction,
                        "trace_sha256": trace_sha256,
                        "replayable": has_snap && has_replay,
                        "created_at": created_at,
                    }));
                }
                Ok(json!({
                    "project_id": project_id,
                    "project": workspace.display().to_string(),
                    "count": traces.len(),
                    "traces": traces,
                }))
            }
            "show" => {
                let generation = opts
                    .generation
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| HarnessError::invalid("traces --action show requires --generation."))?;
                let db = self.store.connection()?;
                let mut trace_row = db::trace(&db, generation)?
                    .ok_or_else(|| HarnessError::missing("Trace not found."))?;
                let has_snap = snapshot::load(&self.store.state, generation)
                    .is_ok_and(|s| s.is_some());
                let has_replay = replay_result::available(&self.store.state, generation);
                if let Some(obj) = trace_row.as_object_mut() {
                    obj.insert("replayable".into(), json!(has_snap && has_replay));
                }
                Ok(trace_row)
            }
            "ingest" => {
                let path = opts.trace_path.as_deref().ok_or_else(|| {
                    HarnessError::invalid("traces --action ingest requires trace_path.")
                })?;
                self.ingest_trace_file(project_id, &workspace, path, opts.generation.as_deref(), opts.skill.as_deref())
            }
            "delete" => {
                let generation = opts
                    .generation
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| HarnessError::invalid("traces --action delete requires --generation."))?;
                let db = self.store.connection()?;
                if db::trace(&db, generation)?.is_none() {
                    return Err(HarnessError::missing("Trace not found."));
                }
                db::delete_trace(&db, generation)?;
                drop(db);
                let _ = snapshot::remove(&self.store.state, generation);
                Ok(json!({"generation": generation, "deleted": true}))
            }
            other => Err(HarnessError::invalid(format!(
                "Unknown traces action `{other}`; expected list, show, ingest, or delete."
            ))),
        }
    }

    fn ingest_trace_file(
        &self,
        project_id: &str,
        workspace: &Path,
        trace_path: &Path,
        explicit_gen: Option<&str>,
        explicit_skill: Option<&str>,
    ) -> Result<Value> {
        let abs_trace = custody::fs::canonicalize(trace_path)?;
        let bytes = custody::fs::read_regular(&abs_trace)?;
        let raw: Value = crate::host::strict_json::parse(&bytes, 4 * 1024 * 1024)?;
        if !raw.is_object() {
            return Err(HarnessError::invalid("Trace file must contain a JSON object."));
        }
        let generation = explicit_gen
            .or_else(|| raw["generation"].as_str())
            .filter(|s| !s.trim().is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("gen-{}", &store::hexid()[..16]));
        let provider = raw["provider"].as_str().unwrap_or("claude");
        let model = raw["model"].as_str();
        let request_raw = raw["request"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| HarnessError::invalid("Trace JSON requires non-empty `request` string."))?;
        let session = self.store.ensure_session_for_project(
            project_id,
            raw["session_id"].as_str(),
            Some(provider),
            model,
        )?;
        let session_id = session["id"]
            .as_str()
            .ok_or_else(|| HarnessError::unavailable("Session missing id."))?;

        let steps_arr = raw["steps"].as_array().cloned().unwrap_or_default();
        if steps_arr.is_empty() {
            return Err(HarnessError::invalid("Trace JSON requires at least one entry in `steps`."));
        }
        let redactor = trace::SessionRedactor::new(workspace, Vec::<(String, String)>::new());
        let trace_parsed = if let Some(first) = steps_arr.first()
            && first.get("kind").and_then(Value::as_str).is_some()
        {
            let (redacted_steps, changed) = redact_trace_value_array(steps_arr, &redactor);
            let counters = crate::core::trace::Counters {
                redacted: changed,
                ..Default::default()
            };
            Trace {
                provider: provider.into(),
                steps: redacted_steps,
                skills: vec![],
                counters,
                complete: true,
            }
        } else {
            // Raw provider stream events array
            let mut rec = Recorder::new(provider);
            for ev in &steps_arr {
                rec.observe(ev);
            }
            rec.finish(&redactor)
        };

        let mut skill_refs = raw["skill_refs"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut trace_for_resolve = trace_parsed.clone();
        if skill_refs.is_empty()
            && let Some(skill_arg) = explicit_skill.or_else(|| raw["skill"].as_str())
        {
            trace_for_resolve.skills.push(crate::core::trace::SkillRef {
                name: Some(skill_arg.to_owned()),
                path: None,
            });
            skill_refs = trace::resolve_skills(
                workspace,
                &ResolvedCommand::default(),
                request_raw,
                &trace_for_resolve,
            );
        }
        let skill_refs = enrich_skill_refs(workspace, skill_refs)?;
        if skill_refs.is_empty() {
            return Err(HarnessError::invalid(
                "Trace references no resolvable SKILL.md skill in workspace or ~/.claude/skills | ~/.agents/skills; provide skill_refs or --skill.",
            ));
        }

        let before = replay_result::capture_with(workspace, Some(&self.store.state), &[])?;
        let _ = snapshot::take(&self.store.state, workspace, &generation)?;
        let observed_out = raw["observed_output"]
            .as_str()
            .or_else(|| raw["output"].as_str());
        replay_result::record_with_output(
            &self.store.state,
            workspace,
            &generation,
            before,
            observed_out,
            &[],
        )?;

        let (redacted_req, _) = redactor.redact(request_raw);
        let mut redaction = trace_parsed.redaction();
        redaction["complete"] = json!(true);
        let db = self.store.connection()?;
        db::save_trace(
            &db,
            &db::TraceRow {
                generation: &generation,
                session_id,
                provider,
                model,
                request: &redacted_req,
                skill_refs: &Value::Array(skill_refs.clone()),
                steps: &Value::Array(trace_parsed.steps),
                usage: raw.get("usage").unwrap_or(&Value::Null),
                redaction: &redaction,
            },
        )?;
        Ok(json!({
            "generation": generation,
            "session_id": session_id,
            "project_id": project_id,
            "skill_refs": skill_refs,
            "replayable": true,
        }))
    }

    pub async fn compile(&self, opts: &CompileOptions) -> Result<Value> {
        let project_rec = self.store.register(&opts.project)?;
        let project_id = project_rec["id"]
            .as_str()
            .ok_or_else(|| HarnessError::unavailable("Project registration returned no id."))?
            .to_owned();
        let workspace = PathBuf::from(
            project_rec["path"]
                .as_str()
                .ok_or_else(|| HarnessError::unavailable("Missing registered project path."))?,
        );

        if let Some(candidate_path) = &opts.candidate_path {
            return self
                .compile_candidate_path(&project_id, &workspace, candidate_path, opts)
                .await;
        }

        let generation = if let Some(trace_path) = &opts.trace_path {
            let ingested = self.ingest_trace_file(
                &project_id,
                &workspace,
                trace_path,
                opts.generation.as_deref(),
                opts.skill.as_deref(),
            )?;
            ingested["generation"]
                .as_str()
                .ok_or_else(|| HarnessError::unavailable("Ingested trace missing generation."))?
                .to_owned()
        } else if let Some(generation_opt) = opts.generation.as_deref().filter(|s| !s.trim().is_empty()) {
            generation_opt.to_owned()
        } else if let Some(skill_name) = opts.skill.as_deref().filter(|s| !s.trim().is_empty()) {
            let traces_list = self.traces(&TracesOptions {
                project: workspace.clone(),
                action: "list".into(),
                generation: None,
                trace_path: None,
                skill: Some(skill_name.to_owned()),
            })?;
            traces_list["traces"]
                .as_array()
                .and_then(|arr| arr.iter().find(|t| t["replayable"] == true))
                .and_then(|t| t["generation"].as_str())
                .map(ToOwned::to_owned)
                .ok_or_else(|| {
                    HarnessError::missing(format!(
                        "No replayable trace recorded for skill `{skill_name}` in this project. Ingest a trace with `traces --action ingest --trace-path <file>` or pass `--candidate <package-or-json>`."
                    ))
                })?
        } else {
            return Err(HarnessError::invalid(
                "Provide generation, trace_path, candidate_path, or skill to compile.",
            ));
        };

        let stored = db::trace(&*self.store.connection()?, &generation)?
            .ok_or_else(|| HarnessError::missing("Trace not found."))?;
        let trace_session_id = stored["session_id"].as_str().unwrap_or("");
        let session = self.store.ensure_session_for_project(
            &project_id,
            (!trace_session_id.is_empty()).then_some(trace_session_id),
            opts.provider.as_deref().or_else(|| stored["provider"].as_str()),
            opts.model.as_deref().or_else(|| stored["model"].as_str()),
        )?;
        let _session_id = session["id"]
            .as_str()
            .ok_or_else(|| HarnessError::unavailable("Session missing id."))?;

        let provider = opts
            .provider
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                session["skillcache"]["compiler"]["provider"]
                    .as_str()
                    .unwrap_or("claude")
            })
            .to_owned();
        if !["claude", "codex"].contains(&provider.as_str()) {
            return Err(HarnessError::invalid(format!(
                "Unsupported compiler provider `{provider}`; expected `claude` or `codex`."
            )));
        }
        let compiler_model = opts.model.clone();
        let mut compiler_override = json!({"provider": provider});
        if let Some(ref model) = compiler_model {
            compiler_override["model"] = json!(model);
        }

        let captured_job: Arc<std::sync::Mutex<Option<compile::Job>>> =
            Arc::new(std::sync::Mutex::new(None));
        let job_slot = captured_job.clone();
        let queued = crate::engine::enqueue_compile_for_trace(
            &self.runtime,
            &session,
            &generation,
            Some(&compiler_override),
            None,
            &|job| {
                let mut guard = job_slot
                    .lock()
                    .map_err(|_| HarnessError::unavailable("Compile job slot poisoned."))?;
                *guard = Some(job);
                Ok(())
            },
        )?;
        let job = captured_job
            .lock()
            .map_err(|_| HarnessError::unavailable("Compile job slot poisoned."))?
            .take()
            .ok_or_else(|| HarnessError::unavailable("Failed to build compile job."))?;
        compile::run(&self.runtime, &job).await;

        let compile_id = queued["compile_id"].as_str().unwrap_or("");
        let capsule_id = queued["capsule_id"].as_str().unwrap_or("");
        let db = self.store.connection()?;
        let compile_row = db::compile(&db, compile_id)?.unwrap_or(Value::Null);
        let capsule_row = db::capsule(&db, capsule_id)?.unwrap_or(Value::Null);
        let gates = db::gates(&db, capsule_id)?;
        Ok(json!({
            "compile_id": compile_id,
            "capsule_id": capsule_id,
            "generation": generation,
            "project_id": project_id,
            "status": compile_row["status"],
            "outcome": compile_row["outcome"],
            "rounds": compile_row["rounds"],
            "compile": compile_row,
            "capsule": capsule_row,
            "gates": gates,
        }))
    }

    async fn compile_candidate_path(
        &self,
        project_id: &str,
        workspace: &Path,
        candidate_path: &Path,
        opts: &CompileOptions,
    ) -> Result<Value> {
        let abs = custody::fs::canonicalize(candidate_path)?;
        if custody::inside_plugin_dir(&abs) {
            return Err(HarnessError::coded(
                crate::error::ErrorKind::Policy,
                "plugin-cwd",
                "Refusing candidate package inside the skillcache plugin directory.",
            ));
        }

        let (mut capsule_val, mut files, examples_val, form, observed_out) = if abs.is_dir() {
            let cap_raw = custody::fs::read_regular(&abs.join(package::CAPSULE_FILE))?;
            let cap_val: Value =
                crate::host::strict_json::parse(&cap_raw, 1024 * 1024)?;
            let pkg = package::load(&abs)?;
            let ex_path = abs.join(package::EXAMPLES_FILE);
            let ex = if ex_path.is_file() {
                let raw = custody::fs::read_regular(&ex_path)?;
                Some(Examples::parse(&raw).map_err(|_| HarnessError::invalid("Invalid examples JSON."))?)
            } else {
                None
            };
            let form = if pkg.markdown.is_some() {
                Form::Textual
            } else {
                Form::Hybrid
            };
            (cap_val, pkg.files, ex, form, None)
        } else {
            let bytes = custody::fs::read_regular(&abs)?;
            let raw: Value = crate::host::strict_json::parse(&bytes, 4 * 1024 * 1024)?;
            let cap_val = raw
                .get("capsule")
                .filter(|v| v.is_object())
                .cloned()
                .ok_or_else(|| HarnessError::invalid("Candidate JSON requires `capsule` object."))?;
            let form = match raw["form"].as_str().or_else(|| cap_val["form"].as_str()) {
                Some("textual") => Form::Textual,
                _ => Form::Hybrid,
            };
            let files = if raw.get("files").is_some_and(Value::is_array) {
                package::Files::from_response(&raw["files"])
            } else if let Some(obj) = raw.get("files").and_then(Value::as_object) {
                let mut f = package::Files::new();
                for (path, content) in obj {
                    let text = content
                        .as_str()
                        .ok_or_else(|| HarnessError::invalid("Candidate file contents must be UTF-8 strings."))?;
                    f.insert(path, text.as_bytes().to_vec());
                }
                f
            } else {
                package::Files::new()
            };
            let examples_val = if raw.get("examples").is_some_and(|v| !v.is_null()) {
                let ex_bytes = serde_json::to_vec(&raw["examples"])?;
                Some(Examples::parse(&ex_bytes).map_err(|_| HarnessError::invalid("Invalid `examples` in candidate JSON."))?)
            } else {
                None
            };
            let observed_out = raw["observed_output"]
                .as_str()
                .map(ToOwned::to_owned);
            (cap_val, files, examples_val, form, observed_out)
        };

        // Ensure capsule carries schema_version & resolved skill tree SHA under `source.skills`
        capsule_val["schema_version"] = crate::core::SCHEMA_VERSION.into();
        let mut raw_skills: Vec<Value> = capsule_val["source"]["skills"]
            .as_array()
            .or_else(|| capsule_val["skills"].as_array())
            .cloned()
            .unwrap_or_default();
        if raw_skills.is_empty()
            && let Some(skill_hint) = opts.skill.as_deref()
        {
            raw_skills.push(json!({"name": skill_hint}));
        }
        let resolved_skill_vals = enrich_skill_refs(workspace, raw_skills)?;
        if resolved_skill_vals.is_empty() {
            return Err(HarnessError::invalid(
                "Candidate capsule must reference an existing skill under workspace/.claude|agents|cursor/skills or specify `skill`.",
            ));
        }
        let skills: Vec<KnownSkill> = resolved_skill_vals
            .iter()
            .filter_map(|v| {
                Some(KnownSkill {
                    name: v["name"].as_str()?.to_owned(),
                    path: v["path"].as_str()?.to_owned(),
                    tree_sha256: v["tree_sha256"].as_str()?.to_owned(),
                })
            })
            .collect();
        if let Some(obj) = capsule_val.as_object_mut() {
            obj.remove("skills");
        }
        capsule_val["source"] = json!({"skills": resolved_skill_vals});
        let parsed_capsule = SkillCapsule::from_value(&capsule_val).map_err(|violations| {
            let msg = violations
                .iter()
                .map(|v| format!("{}: {}", v.code, v.message))
                .collect::<Vec<_>>()
                .join("; ");
            HarnessError::invalid(format!("Candidate capsule failed validation: {msg}"))
        })?;
        let capsule_bytes =
            serde_json::to_vec_pretty(&capsule_val)?;
        if form == Form::Hybrid && files.get(package::CAPSULE_FILE).is_none() {
            // Files map keeps program/asset entries only; check textual below.
            let _ = &mut files;
        }
        let markdown = if form == Form::Textual {
            files
                .get(package::MARKDOWN_FILE)
                .map(|b| String::from_utf8_lossy(b).into_owned())
        } else {
            None
        };
        let candidate = Candidate {
            capsule: parsed_capsule,
            capsule_bytes,
            files,
            markdown,
            examples: examples_val.clone(),
        };

        let generation = opts
            .generation
            .clone()
            .unwrap_or_else(|| format!("cand-{}", &store::hexid()[..16]));
        let session = self.store.ensure_session_for_project(
            project_id,
            None,
            opts.provider.as_deref(),
            opts.model.as_deref(),
        )?;
        let session_id = session["id"].as_str().unwrap_or("cli").to_owned();
        let compile_id = store::id();
        let capsule_id = store::id();

        let trace_steps = candidate
            .capsule
            .programs
            .iter()
            .enumerate()
            .map(|(i, p)| {
                json!({
                    "kind": "tool",
                    "seq": i + 1,
                    "tool": "Bash",
                    "input": {"command": p.entry.first().cloned().unwrap_or_default()},
                    "output": {},
                })
            })
            .collect::<Vec<_>>();
        let observed_files = candidate
            .capsule
            .skills
            .iter()
            .map(|s| s.path.clone())
            .collect::<Vec<_>>();

        // Capture workspace snapshot and expected observed outcome if not already recorded
        if snapshot::load(&self.store.state, &generation)?.is_none() {
            let before = replay_result::capture_with(workspace, Some(&self.store.state), &[])?;
            let _ = snapshot::take(&self.store.state, workspace, &generation)?;
            let inferred_out = observed_out.or_else(|| {
                examples_val.as_ref().and_then(|ex| {
                    ex.steps
                        .values()
                        .next()
                        .and_then(|v| serde_json::to_string(v).ok())
                })
            });
            replay_result::record_with_output(
                &self.store.state,
                workspace,
                &generation,
                before,
                inferred_out.as_deref(),
                &[],
            )?;
        }

        let db = self.store.connection()?;
        if db::trace(&db, &generation)?.is_none() {
            db::save_trace(
                &db,
                &db::TraceRow {
                    generation: &generation,
                    session_id: &session_id,
                    provider: opts.provider.as_deref().unwrap_or("claude"),
                    model: opts.model.as_deref(),
                    request: "Direct candidate qualification",
                    skill_refs: &json!(resolved_skill_vals),
                    steps: &json!(trace_steps),
                    usage: &Value::Null,
                    redaction: &json!({"complete": true, "mode": "direct_candidate"}),
                },
            )?;
        }
        let primary_skill = json!({
            "name": skills[0].name,
            "path": skills[0].path,
            "tree_sha256": skills[0].tree_sha256,
        });
        db::begin_capsule(
            &db,
            &capsule_id,
            &compile_id,
            project_id,
            &session_id,
            &generation,
            &primary_skill,
            None,
        )?;
        db::begin_compile(
            &db,
            &compile_id,
            &capsule_id,
            project_id,
            &session_id,
            &generation,
            opts.provider.as_deref().unwrap_or("claude"),
            opts.model.as_deref(),
            None,
        )?;
        drop(db);

        let job = compile::Job {
            compile_id: compile_id.clone(),
            capsule_id: capsule_id.clone(),
            session_id,
            generation,
            project_id: project_id.to_owned(),
            workspace: workspace.to_path_buf(),
            provider: opts.provider.as_deref().unwrap_or("claude").to_owned(),
            model: opts.model.clone(),
            eval_arm: None,
        };
        let mut res = compile::qualify_candidate_direct(
            &self.runtime,
            &job,
            candidate,
            form,
            observed_files,
            opts.round_dry_run,
        )
        .await?;
        let capsule_row = self.domain.get(project_id, &capsule_id).ok();
        if let Some(obj) = res.as_object_mut()
            && let Some(cap) = capsule_row
        {
            obj.insert("capsule".into(), cap);
        }
        Ok(res)
    }
}

fn summarize_capsules(capsules: &[Value]) -> (Value, Value) {
    let mut counts = serde_json::Map::from_iter([
        ("active".into(), json!(0u64)),
        ("compiling".into(), json!(0u64)),
        ("stale".into(), json!(0u64)),
        ("needs_recompile".into(), json!(0u64)),
        ("failed".into(), json!(0u64)),
        ("retired".into(), json!(0u64)),
        ("total".into(), json!(capsules.len() as u64)),
    ]);
    let mut stale_reasons = serde_json::Map::new();
    for cap in capsules {
        let status = cap["status"].as_str().unwrap_or("unknown");
        if let Some(entry) = counts.get_mut(status)
            && let Some(n) = entry.as_u64()
        {
            *entry = json!(n + 1);
        }
        if matches!(status, "stale" | "needs_recompile")
            && let (Some(id), Some(reason)) =
                (cap["capsule_id"].as_str(), cap["reason"].as_str())
        {
            stale_reasons.insert(id.to_owned(), json!(reason));
        }
    }
    (Value::Object(counts), Value::Object(stale_reasons))
}

fn enrich_skill_refs(workspace: &Path, raw: Vec<Value>) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for mut item in raw {
        let path_str = item["path"].as_str().map(ToOwned::to_owned);
        let name_str = item["name"].as_str().map(ToOwned::to_owned);
        if let Some(ref p) = path_str
            && item["tree_sha256"].as_str().is_none_or(|s| s.is_empty())
            && let Some(folder) = trace::skill_file(workspace, p)
                .and_then(|f| f.parent().map(Path::to_path_buf))
        {
            let tree = crate::core::fingerprint::tree_sha256(&folder)?;
            item["tree_sha256"] = json!(tree.sha256);
            if name_str.is_none()
                && let Some(name) = folder.file_name().and_then(|n| n.to_str())
            {
                item["name"] = json!(name);
            }
            out.push(item);
            continue;
        }
        if path_str.is_none()
            && let Some(ref name) = name_str
        {
            let resolved = trace::resolve_skills(
                workspace,
                &ResolvedCommand::default(),
                "",
                &Trace {
                    provider: String::new(),
                    steps: vec![],
                    skills: vec![crate::core::trace::SkillRef {
                        name: Some(name.clone()),
                        path: None,
                    }],
                    counters: Default::default(),
                    complete: true,
                },
            );
            if let Some(first) = resolved.into_iter().next() {
                out.push(first);
                continue;
            }
        }
        if item["name"].as_str().is_some_and(|s| !s.is_empty())
            && item["path"].as_str().is_some_and(|s| !s.is_empty())
            && item["tree_sha256"].as_str().is_some_and(|s| !s.is_empty())
        {
            out.push(item);
        }
    }
    Ok(out)
}

fn redact_trace_value(value: Value, redactor: &dyn Redactor) -> (Value, usize) {
    match value {
        Value::String(s) => {
            let (clean, c) = redactor.redact(&s);
            (Value::String(clean), c)
        }
        Value::Array(items) => {
            let (clean, c) = redact_trace_value_array(items, redactor);
            (Value::Array(clean), c)
        }
        Value::Object(map) => {
            let mut total = 0usize;
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                let (clean_v, c) = redact_trace_value(v, redactor);
                total += c;
                out.insert(k, clean_v);
            }
            (Value::Object(out), total)
        }
        other => (other, 0),
    }
}

fn redact_trace_value_array(items: Vec<Value>, redactor: &dyn Redactor) -> (Vec<Value>, usize) {
    let mut total = 0usize;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let (clean, c) = redact_trace_value(item, redactor);
        total += c;
        out.push(clean);
    }
    (out, total)
}
