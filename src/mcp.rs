//! Bounded stdio JSON-RPC 2.0 MCP server (2025-06-18) exposing all 9 `skillcache_*` tools.
use crate::custody;
use crate::engine::facade::{
    CompileOptions, ListCapsulesOptions, SkillCacheEngine, TracesOptions,
};
use crate::error::{HarnessError, Result};
use serde_json::{Value, json, value::RawValue};
use std::{
    collections::BTreeMap,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const MAX_MESSAGE: usize = 1024 * 1024;
pub const MAX_DEPTH: usize = 64;

pub fn tools() -> Vec<Value> {
    let spec = |name: &str,
                description: &str,
                properties: Value,
                required: Vec<&str>,
                read_only: bool,
                destructive: bool| {
        json!({
            "name": name,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false
            },
            "annotations": {
                "readOnlyHint": read_only,
                "destructiveHint": destructive,
                "openWorldHint": false
            }
        })
    };

    let project_prop = json!({
        "type": "string",
        "description": "Explicit target repository path (required when server cwd is inside the packaged plugin directory)."
    });
    let state_dir_prop = json!({
        "type": "string",
        "description": "Optional SkillCache state directory override (defaults to $SKILLCACHE_STATE_DIR or ~/.local/state/skillcache)."
    });

    vec![
        spec(
            "skillcache_status",
            "Inspect SkillCache runtime readiness (sandbox availability, compiler CLIs, state dir, capsule counts, trace counts, amortization) for a target project.",
            json!({
                "project": project_prop,
                "state_dir": state_dir_prop,
            }),
            vec![],
            true,
            false,
        ),
        spec(
            "skillcache_capsules_list",
            "List SkillCache capsules registered for a project, including status, form, stale reasons, and amortization.",
            json!({
                "project": project_prop,
                "state_dir": state_dir_prop,
                "skill": {"type": "string", "description": "Optional skill name or path filter."},
                "status": {"type": "string", "enum": ["compiling", "active", "stale", "needs_recompile", "failed", "retired"], "description": "Optional status filter."},
                "form": {"type": "string", "enum": ["hybrid", "textual"], "description": "Optional capsule form filter."},
                "include_retired": {"type": "boolean", "description": "Include retired capsules in results."}
            }),
            vec![],
            true,
            false,
        ),
        spec(
            "skillcache_capsule_inspect",
            "Inspect detailed metadata, gate verification reports, dependency manifest, and run history for a capsule.",
            json!({
                "capsule_id": {"type": "string", "description": "Capsule identifier to inspect."},
                "project": project_prop,
                "state_dir": state_dir_prop,
            }),
            vec!["capsule_id"],
            true,
            false,
        ),
        spec(
            "skillcache_gate1",
            "Run static structural, allowlist, dataflow, and secret-safety Gate 1 validation on a capsule package directory.",
            json!({
                "package_dir": {"type": "string", "description": "Explicit path to a SkillCache capsule package directory containing capsule.json."},
                "examples_path": {"type": "string", "description": "Optional path to an examples JSON file to validate bindings against."}
            }),
            vec!["package_dir"],
            true,
            false,
        ),
        spec(
            "skillcache_compile",
            "Compile or qualify a SkillCache capsule from an observed trace generation, trace JSON file, direct candidate package/JSON, or skill name, running Gate 1, Gate 2 (--unshare-net sandbox), and Gate 3 replay.",
            json!({
                "project": project_prop,
                "state_dir": state_dir_prop,
                "generation": {"type": "string", "description": "Observed trace generation id to compile."},
                "skill": {"type": "string", "description": "Skill name or SKILL.md path when selecting a trace or resolving candidate metadata."},
                "trace_path": {"type": "string", "description": "Path to a recorded trace JSON file to ingest and compile."},
                "candidate_path": {"type": "string", "description": "Path to a pre-authored candidate JSON file or package directory to qualify directly through Gate 1/2/3."},
                "provider": {"type": "string", "enum": ["claude", "codex"], "description": "Compiler provider CLI (defaults to claude)."},
                "model": {"type": "string", "description": "Optional model override for the compiler CLI."},
                "round_dry_run": {"type": "boolean", "description": "Allow empty dry-run examples when qualifying a direct candidate."}
            }),
            vec![],
            false,
            false,
        ),
        spec(
            "skillcache_requalify",
            "Re-run Gate 1, Gate 2, and Gate 3 over an existing compiled capsule against the current skill and workspace without invoking an LLM.",
            json!({
                "capsule_id": {"type": "string", "description": "Capsule identifier to requalify."},
                "project": project_prop,
                "state_dir": state_dir_prop,
            }),
            vec!["capsule_id"],
            false,
            false,
        ),
        spec(
            "skillcache_retire",
            "Retire a compiled capsule with an optional reason so it is no longer selected for warm runs.",
            json!({
                "capsule_id": {"type": "string", "description": "Capsule identifier to retire."},
                "reason": {"type": "string", "description": "Optional human-readable retirement reason."},
                "project": project_prop,
                "state_dir": state_dir_prop,
            }),
            vec!["capsule_id"],
            false,
            true,
        ),
        spec(
            "skillcache_invalidate",
            "Mark an active or needs_recompile capsule as stale so it requires requalification before warm reuse.",
            json!({
                "capsule_id": {"type": "string", "description": "Capsule identifier to invalidate."},
                "project": project_prop,
                "state_dir": state_dir_prop,
            }),
            vec!["capsule_id"],
            false,
            false,
        ),
        spec(
            "skillcache_traces",
            "List, inspect, ingest, or delete SkillCache cold-turn observation traces for a project.",
            json!({
                "project": project_prop,
                "state_dir": state_dir_prop,
                "action": {"type": "string", "enum": ["list", "show", "ingest", "delete"], "description": "Trace operation (defaults to list)."},
                "generation": {"type": "string", "description": "Trace generation id (required for show/delete; optional override for ingest)."},
                "trace_path": {"type": "string", "description": "Path to a trace JSON file (required for ingest)."},
                "skill": {"type": "string", "description": "Optional skill filter for list or fallback skill resolution for ingest."}
            }),
            vec![],
            false,
            false,
        ),
    ]
}

fn validate_args(spec: &Value, input: &Value) -> Result<()> {
    let obj = input
        .as_object()
        .ok_or_else(|| HarnessError::invalid("Tool arguments must be a JSON object"))?;
    let props = spec["inputSchema"]["properties"]
        .as_object()
        .ok_or_else(|| HarnessError::invalid("Invalid tool schema"))?;
    if obj.keys().any(|k| !props.contains_key(k)) {
        return Err(HarnessError::invalid("Unknown tool argument supplied"));
    }
    if spec["inputSchema"]["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|req| !obj.contains_key(req))
    {
        return Err(HarnessError::invalid("Missing required tool argument"));
    }
    Ok(())
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(HarnessError::invalid(format!(
            "Argument `{key}` must be a string."
        ))),
    }
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| HarnessError::invalid(format!("Argument `{key}` must be a non-empty string.")))
}

fn opt_bool(args: &Value, key: &str, default: bool) -> Result<bool> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(HarnessError::invalid(format!(
            "Argument `{key}` must be a boolean."
        ))),
    }
}

fn resolve_input_file(base: Option<&Path>, raw: &str) -> Result<PathBuf> {
    let raw_cwd = std::env::current_dir()?;
    let canon_cwd = std::fs::canonicalize(&raw_cwd).unwrap_or_else(|_| raw_cwd.clone());
    let candidate = PathBuf::from(raw);
    let resolved = if candidate.is_absolute() {
        candidate
    } else if let Some(project) = base {
        project.join(candidate)
    } else {
        if custody::inside_plugin_dir(&raw_cwd) || custody::inside_plugin_dir(&canon_cwd) {
            return Err(HarnessError::coded(
                crate::error::ErrorKind::Policy,
                "plugin-cwd",
                "Refusing relative path while cwd is inside the skillcache plugin directory.",
            ));
        }
        raw_cwd.join(candidate)
    };
    let resolved_canon = std::fs::canonicalize(&resolved).unwrap_or_else(|_| resolved.clone());
    if custody::inside_plugin_dir(&resolved) || custody::inside_plugin_dir(&resolved_canon) {
        return Err(HarnessError::coded(
            crate::error::ErrorKind::Policy,
            "plugin-cwd",
            "Refusing path inside the skillcache plugin directory.",
        ));
    }
    Ok(resolved)
}

fn resolve_state_dir(args: &Value) -> Result<Option<PathBuf>> {
    let Some(raw) = opt_str(args, "state_dir")? else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(trimmed);
    let abs = if path.is_absolute() {
        path
    } else {
        let raw_cwd = std::env::current_dir()?;
        let canon_cwd = std::fs::canonicalize(&raw_cwd).unwrap_or_else(|_| raw_cwd.clone());
        if custody::inside_plugin_dir(&raw_cwd) || custody::inside_plugin_dir(&canon_cwd) {
            return Err(HarnessError::coded(
                crate::error::ErrorKind::Policy,
                "plugin-cwd",
                "Refusing relative state_dir inside the skillcache plugin directory.",
            ));
        }
        raw_cwd.join(path)
    };
    let abs_canon = std::fs::canonicalize(&abs).unwrap_or_else(|_| abs.clone());
    if custody::inside_plugin_dir(&abs) || custody::inside_plugin_dir(&abs_canon) {
        return Err(HarnessError::coded(
            crate::error::ErrorKind::Policy,
            "plugin-cwd",
            "Refusing state_dir inside the skillcache plugin directory.",
        ));
    }
    Ok(Some(abs))
}

fn execute_tool(name: &str, args: &Value, rt: &tokio::runtime::Runtime) -> Result<Value> {
    match name {
        "skillcache_gate1" => {
            let pkg_raw = req_str(args, "package_dir")?;
            let pkg_dir = resolve_input_file(None, pkg_raw)?;
            let examples_path = opt_str(args, "examples_path")?
                .map(|p| resolve_input_file(None, p))
                .transpose()?;
            SkillCacheEngine::gate1(&pkg_dir, examples_path.as_deref())
        }
        "skillcache_status" => {
            let project = custody::resolve_project_path(opt_str(args, "project")?)?;
            let state_dir = resolve_state_dir(args)?;
            let engine = SkillCacheEngine::open(state_dir.as_deref())?;
            engine.status(&project)
        }
        "skillcache_capsules_list" => {
            let project = custody::resolve_project_path(opt_str(args, "project")?)?;
            let state_dir = resolve_state_dir(args)?;
            let engine = SkillCacheEngine::open(state_dir.as_deref())?;
            engine.capsules_list(&ListCapsulesOptions {
                project,
                skill: opt_str(args, "skill")?.map(ToOwned::to_owned),
                status: opt_str(args, "status")?.map(ToOwned::to_owned),
                form: opt_str(args, "form")?.map(ToOwned::to_owned),
                include_retired: opt_bool(args, "include_retired", false)?,
            })
        }
        "skillcache_capsule_inspect" => {
            let project = custody::resolve_project_path(opt_str(args, "project")?)?;
            let state_dir = resolve_state_dir(args)?;
            let capsule_id = req_str(args, "capsule_id")?;
            let engine = SkillCacheEngine::open(state_dir.as_deref())?;
            engine.capsule_inspect(&project, capsule_id)
        }
        "skillcache_compile" => {
            let project = custody::resolve_project_path(opt_str(args, "project")?)?;
            let state_dir = resolve_state_dir(args)?;
            let trace_path = opt_str(args, "trace_path")?
                .map(|p| resolve_input_file(Some(&project), p))
                .transpose()?;
            let candidate_path = opt_str(args, "candidate_path")?
                .map(|p| resolve_input_file(Some(&project), p))
                .transpose()?;
            let opts = CompileOptions {
                project,
                generation: opt_str(args, "generation")?.map(ToOwned::to_owned),
                skill: opt_str(args, "skill")?.map(ToOwned::to_owned),
                trace_path,
                candidate_path,
                provider: opt_str(args, "provider")?.map(ToOwned::to_owned),
                model: opt_str(args, "model")?.map(ToOwned::to_owned),
                round_dry_run: opt_bool(args, "round_dry_run", false)?,
            };
            let engine = SkillCacheEngine::open(state_dir.as_deref())?;
            rt.block_on(engine.compile(&opts))
        }
        "skillcache_requalify" => {
            let project = custody::resolve_project_path(opt_str(args, "project")?)?;
            let state_dir = resolve_state_dir(args)?;
            let capsule_id = req_str(args, "capsule_id")?;
            let engine = SkillCacheEngine::open(state_dir.as_deref())?;
            rt.block_on(engine.requalify(&project, capsule_id))
        }
        "skillcache_retire" => {
            let project = custody::resolve_project_path(opt_str(args, "project")?)?;
            let state_dir = resolve_state_dir(args)?;
            let capsule_id = req_str(args, "capsule_id")?;
            let reason = opt_str(args, "reason")?;
            let engine = SkillCacheEngine::open(state_dir.as_deref())?;
            engine.retire(&project, capsule_id, reason)
        }
        "skillcache_invalidate" => {
            let project = custody::resolve_project_path(opt_str(args, "project")?)?;
            let state_dir = resolve_state_dir(args)?;
            let capsule_id = req_str(args, "capsule_id")?;
            let engine = SkillCacheEngine::open(state_dir.as_deref())?;
            engine.invalidate(&project, capsule_id)
        }
        "skillcache_traces" => {
            let project = custody::resolve_project_path(opt_str(args, "project")?)?;
            let state_dir = resolve_state_dir(args)?;
            let action = opt_str(args, "action")?.unwrap_or("list").to_owned();
            let trace_path = opt_str(args, "trace_path")?
                .map(|p| resolve_input_file(Some(&project), p))
                .transpose()?;
            let engine = SkillCacheEngine::open(state_dir.as_deref())?;
            engine.traces(&TracesOptions {
                project,
                action,
                generation: opt_str(args, "generation")?.map(ToOwned::to_owned),
                trace_path,
                skill: opt_str(args, "skill")?.map(ToOwned::to_owned),
            })
        }
        _ => Err(HarnessError::not_found(format!(
            "Unknown SkillCache tool: {name}"
        ))),
    }
}

fn check_depth(value: &RawValue, limit: usize) -> Result<Value> {
    let raw = value.get();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape = false;
    for byte in raw.bytes() {
        if in_string {
            if escape {
                escape = false;
            } else if byte == b'\\' {
                escape = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > limit {
                    return Err(HarnessError::invalid("JSON depth limit exceeded."));
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    serde_json::from_str::<Value>(raw).map_err(HarnessError::from)
}

#[derive(PartialEq, Eq)]
enum Framing {
    Line,
    ContentLength,
}

fn read_frame(
    reader: &mut impl BufRead,
    framing: &mut Option<Framing>,
) -> std::io::Result<Option<String>> {
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            let len: usize = rest
                .trim()
                .parse()
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad length"))?;
            if len > MAX_MESSAGE {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "message exceeds byte limit",
                ));
            }
            loop {
                let mut hdr = String::new();
                if reader.read_line(&mut hdr)? == 0 || hdr.trim().is_empty() {
                    break;
                }
            }
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf)?;
            *framing = Some(Framing::ContentLength);
            let body = String::from_utf8(buf)
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "non-utf8"))?;
            return Ok(Some(body));
        }
        if line.len() > MAX_MESSAGE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "line exceeds byte limit",
            ));
        }
        *framing = Some(Framing::Line);
        return Ok(Some(line));
    }
}

fn write_frame(
    writer: &mut impl Write,
    framing: Framing,
    payload: &Value,
) -> std::io::Result<()> {
    let body = serde_json::to_string(payload)?;
    if framing == Framing::ContentLength {
        write!(writer, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
    } else {
        writeln!(writer, "{body}")?;
    }
    writer.flush()
}

pub fn serve() -> Result<()> {
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|e| HarnessError::unavailable(format!("Failed to initialize tokio runtime: {e}")))?,
    );
    let specs: BTreeMap<String, Value> = tools()
        .into_iter()
        .filter_map(|t| Some((t["name"].as_str()?.to_owned(), t)))
        .collect();
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = std::io::BufReader::new(stdin.lock());
    let mut writer = stdout.lock();
    let mut framing = None;

    while let Some(frame) = read_frame(&mut reader, &mut framing)? {
        let active_framing = if framing == Some(Framing::ContentLength) {
            Framing::ContentLength
        } else {
            Framing::Line
        };
        let raw: Box<RawValue> = match serde_json::from_str(&frame) {
            Ok(v) => v,
            Err(_) => {
                write_frame(
                    &mut writer,
                    active_framing,
                    &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}),
                )?;
                continue;
            }
        };
        let req = match check_depth(&raw, MAX_DEPTH) {
            Ok(v) => v,
            Err(e) => {
                write_frame(
                    &mut writer,
                    active_framing,
                    &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":e.message}}),
                )?;
                continue;
            }
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let has_id = req.get("id").is_some();
        let method = req["method"].as_str().unwrap_or("");
        if method == "notifications/initialized" || method == "notifications/cancelled" {
            continue;
        }
        if !has_id {
            continue;
        }
        let response = match method {
            "initialize" => {
                let req_proto = req["params"]["protocolVersion"].as_str();
                let proto = match req_proto {
                    Some(v) if !v.trim().is_empty() && (v <= PROTOCOL_VERSION || v == PROTOCOL_VERSION) => v,
                    _ => PROTOCOL_VERSION,
                };
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": proto,
                        "capabilities": {"tools": {"listChanged": false}},
                        "serverInfo": {"name": "skillcache", "version": env!("CARGO_PKG_VERSION")}
                    }
                })
            }
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"tools": tools()}
            }),
            "tools/call" => {
                let tool_name = req["params"]["name"].as_str().unwrap_or("");
                let args = req["params"].get("arguments").cloned().unwrap_or_else(|| json!({}));
                let call_res = match specs.get(tool_name) {
                    None => Err(HarnessError::not_found(format!(
                        "Unknown tool: {tool_name}"
                    ))),
                    Some(spec) => validate_args(spec, &args)
                        .and_then(|_| execute_tool(tool_name, &args, &runtime)),
                };
                match call_res {
                    Ok(value) => {
                        let text = serde_json::to_string_pretty(&value)?;
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{"type": "text", "text": text}],
                                "structuredContent": value,
                                "isError": false
                            }
                        })
                    }
                    Err(err) => {
                        let err_json = json!({
                            "error": err.message,
                            "code": err.code,
                            "status": err.status,
                        });
                        json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "content": [{"type": "text", "text": err.message}],
                                "structuredContent": err_json,
                                "isError": true
                            }
                        })
                    }
                }
            }
            other => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("Method not found: {other}")}
            }),
        };
        write_frame(&mut writer, active_framing, &response)?;
    }
    Ok(())
}
