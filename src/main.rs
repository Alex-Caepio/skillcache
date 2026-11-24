//! SkillCache engine CLI, multicall executor (`skillcache-run`/`skillcache-tool`), internal sandbox probes, and stdio MCP server.
use serde_json::{Value, json};
use skillcache::core::{cli, env_filter};
use skillcache::custody;
use skillcache::engine::facade::{
    CompileOptions, ListCapsulesOptions, SkillCacheEngine, TracesOptions,
};
use skillcache::error::{HarnessError, Result};
use skillcache::host;
use std::path::PathBuf;

const USAGE: &str = "\
skillcache-engine — Standalone SkillCache compiler, sandbox gate runner, capsule registry & MCP server

USAGE:
  skillcache-engine <COMMAND> [OPTIONS]
  skillcache-run <run|continue|status|reset> ...     (multicall argv[0] executor alias)
  skillcache-tool <program> --input <JSON> | --check  (multicall argv[0] program tool alias)

EXECUTOR & GATE COMMANDS:
  run ['<JSON>'] [--replay <examples.json>]     Start a staged capsule plan
  continue ['<JSON>']                           Resume a capsule plan with supplied input
  reset                                         Reset active warm executor session state
  tool <program> (--input <JSON> | --check)     Execute or check one capsule program
  gate1 <package_dir> [--examples <file>]       Run static Gate 1 validation on a package

MANAGEMENT COMMANDS:
  status [--project <dir>] [--state-dir <dir>]  Inspect sandbox, compilers, capsules & amortization
  capsules [--project <dir>] [--state-dir <dir>] [--skill <s>] [--status <s>] [--form <f>] [--include-retired]
  inspect <capsule_id> [--project <dir>] [--state-dir <dir>]
  compile [--project <dir>] [--state-dir <dir>] [--generation <id> | --trace-path <file> | --candidate <path> | --skill <s>] [--provider claude|codex] [--model <m>] [--dry-run]
  requalify <capsule_id> [--project <dir>] [--state-dir <dir>]
  retire <capsule_id> [--reason <text>] [--project <dir>] [--state-dir <dir>]
  invalidate <capsule_id> [--project <dir>] [--state-dir <dir>]
  traces [--action list|show|ingest|delete] [--project <dir>] [--state-dir <dir>] [--generation <id>] [--trace-path <file>] [--skill <s>]
  mcp                                           Start stdio JSON-RPC 2025-06-18 MCP server
";

fn print_json(value: &Value) {
    if let Ok(pretty) = serde_json::to_string_pretty(value) {
        println!("{pretty}");
    } else {
        println!("{value}");
    }
}

fn die_err(err: &HarnessError) -> i32 {
    eprintln!("error: {}", err.message);
    println!(
        "{}",
        json!({"error": err.message, "code": err.code, "status": err.status})
    );
    if err.status == 400 { 2 } else { 1 }
}

fn take_opt_val(args: &[String], i: &mut usize, flag: &str) -> Result<String> {
    if let Some((_, rhs)) = args[*i].split_once('=') {
        if rhs.is_empty() {
            return Err(HarnessError::invalid(format!(
                "Flag `{flag}` requires a value."
            )));
        }
        return Ok(rhs.to_owned());
    }
    let next = args
        .get(*i + 1)
        .ok_or_else(|| HarnessError::invalid(format!("Flag `{flag}` requires a value.")))?;
    *i += 1;
    Ok(next.clone())
}

#[derive(Default)]
struct CliFlags {
    positional: Vec<String>,
    project: Option<String>,
    state_dir: Option<PathBuf>,
    capsule_id: Option<String>,
    generation: Option<String>,
    skill: Option<String>,
    status: Option<String>,
    form: Option<String>,
    include_retired: bool,
    trace_path: Option<PathBuf>,
    candidate_path: Option<PathBuf>,
    examples_path: Option<PathBuf>,
    provider: Option<String>,
    model: Option<String>,
    reason: Option<String>,
    action: Option<String>,
    dry_run: bool,
}

fn parse_flags(args: &[String]) -> Result<CliFlags> {
    let mut out = CliFlags::default();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let flag = arg.split('=').next().unwrap_or(arg.as_str());
        match flag {
            "--project" | "-p" => out.project = Some(take_opt_val(args, &mut i, flag)?),
            "--state-dir" => out.state_dir = Some(PathBuf::from(take_opt_val(args, &mut i, flag)?)),
            "--capsule-id" => out.capsule_id = Some(take_opt_val(args, &mut i, flag)?),
            "--generation" => out.generation = Some(take_opt_val(args, &mut i, flag)?),
            "--skill" => out.skill = Some(take_opt_val(args, &mut i, flag)?),
            "--status" => out.status = Some(take_opt_val(args, &mut i, flag)?),
            "--form" => out.form = Some(take_opt_val(args, &mut i, flag)?),
            "--include-retired" => out.include_retired = true,
            "--trace-path" => out.trace_path = Some(PathBuf::from(take_opt_val(args, &mut i, flag)?)),
            "--candidate" | "--candidate-path" => {
                out.candidate_path = Some(PathBuf::from(take_opt_val(args, &mut i, flag)?))
            }
            "--examples" | "--examples-path" => {
                out.examples_path = Some(PathBuf::from(take_opt_val(args, &mut i, flag)?))
            }
            "--provider" => out.provider = Some(take_opt_val(args, &mut i, flag)?),
            "--model" => out.model = Some(take_opt_val(args, &mut i, flag)?),
            "--reason" => out.reason = Some(take_opt_val(args, &mut i, flag)?),
            "--action" => out.action = Some(take_opt_val(args, &mut i, flag)?),
            "--dry-run" | "--round-dry-run" => out.dry_run = true,
            "--json" => {}
            other if other.starts_with('-') => {
                return Err(HarnessError::invalid(format!("Unknown flag: {other}")));
            }
            _ => out.positional.push(arg.clone()),
        }
        i += 1;
    }
    Ok(out)
}

fn require_capsule_id(flags: &CliFlags) -> Result<String> {
    flags
        .capsule_id
        .clone()
        .or_else(|| flags.positional.first().cloned())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| HarnessError::invalid("A capsule_id positional argument or --capsule-id is required."))
}

fn check_state_dir(dir: &Option<PathBuf>) -> Result<Option<PathBuf>> {
    match dir {
        None => Ok(None),
        Some(p) => {
            let abs = custody::absolute_lexical(p)?;
            if custody::inside_plugin_dir(&abs) {
                return Err(HarnessError::coded(
                    skillcache::error::ErrorKind::Policy,
                    "plugin-cwd",
                    "Refusing state_dir inside the skillcache plugin directory.",
                ));
            }
            Ok(Some(abs))
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // 1. Internal sandbox / guardian child entrypoints precede multicall dispatch.
    if let Some(sub) = args.get(1).map(String::as_str) {
        match sub {
            "internal-guard" => {
                let code = host::process::guard_entry(&args[2..]).unwrap_or_else(|e| {
                    eprintln!("{e}");
                    1
                });
                std::process::exit(code);
            }
            "internal-discovery-probe" => {
                let code = host::sandbox::probe_entry(&args[2..]).unwrap_or_else(|e| {
                    eprintln!("{e}");
                    1
                });
                std::process::exit(code);
            }
            "internal-sandbox-child" => {
                let code = host::sandbox::child_entry(&args[2..]).unwrap_or_else(|e| {
                    eprintln!("{e}");
                    1
                });
                std::process::exit(code);
            }
            _ => {}
        }
    }

    // 2. Multicall argv[0] dispatch for staged warm sessions (`skillcache-run`, `skillcache-tool`).
    if let Some(staged) = cli::staged_tool(args.first().map(std::ffi::OsStr::new)) {
        let code = match staged {
            "run-command" => cli::execute_run(&args[1..]),
            "tool" => cli::execute_tool(&args[1..]),
            _ => 2,
        };
        std::process::exit(code);
    }

    // 3. Regular subcommand dispatch.
    let sub = args.get(1).map(String::as_str).unwrap_or("--help");
    match sub {
        "--help" | "-h" | "help" => {
            println!("{USAGE}");
            std::process::exit(0);
        }
        "--version" | "-V" => {
            println!("skillcache-engine {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        "mcp" => {
            if let Err(err) = skillcache::mcp::serve() {
                eprintln!("skillcache mcp error: {err}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
        "skillcache" => {
            let inner = args.get(2).map(String::as_str).unwrap_or("");
            let rest = if args.len() > 3 { &args[3..] } else { &[] };
            std::process::exit(cli::execute(inner, rest));
        }
        "run" | "continue" | "reset" | "tool" | "gate1" => {
            std::process::exit(cli::execute(sub, &args[2..]));
        }
        "status" => {
            let has_mgmt_flags = args[2..]
                .iter()
                .any(|a| a.starts_with("--project") || a.starts_with("--state-dir") || a == "-p");
            let has_executor_env = std::env::var_os(env_filter::PACKAGE).is_some()
                && std::env::var_os(env_filter::STATE_FILE).is_some();
            if has_executor_env && !has_mgmt_flags {
                std::process::exit(cli::execute("status", &args[2..]));
            }
            let code = match run_status(&args[2..]) {
                Ok(val) => {
                    print_json(&val);
                    0
                }
                Err(err) => die_err(&err),
            };
            std::process::exit(code);
        }
        "capsules" | "inspect" | "compile" | "requalify" | "retire" | "invalidate" | "traces" => {
            let code = match dispatch_mgmt(sub, &args[2..]) {
                Ok(val) => {
                    print_json(&val);
                    0
                }
                Err(err) => die_err(&err),
            };
            std::process::exit(code);
        }
        unknown => {
            eprintln!("Unknown subcommand: {unknown}\n\n{USAGE}");
            std::process::exit(2);
        }
    }
}

fn run_status(args: &[String]) -> Result<Value> {
    let flags = parse_flags(args)?;
    let project = custody::resolve_project_path(flags.project.as_deref())?;
    let state_dir = check_state_dir(&flags.state_dir)?;
    let engine = SkillCacheEngine::open(state_dir.as_deref())?;
    engine.status(&project)
}

fn dispatch_mgmt(sub: &str, args: &[String]) -> Result<Value> {
    let flags = parse_flags(args)?;
    let project = custody::resolve_project_path(flags.project.as_deref())?;
    let state_dir = check_state_dir(&flags.state_dir)?;
    let engine = SkillCacheEngine::open(state_dir.as_deref())?;
    match sub {
        "capsules" => engine.capsules_list(&ListCapsulesOptions {
            project,
            skill: flags.skill,
            status: flags.status,
            form: flags.form,
            include_retired: flags.include_retired,
        }),
        "inspect" => {
            let id = require_capsule_id(&flags)?;
            engine.capsule_inspect(&project, &id)
        }
        "retire" => {
            let id = require_capsule_id(&flags)?;
            engine.retire(&project, &id, flags.reason.as_deref())
        }
        "invalidate" => {
            let id = require_capsule_id(&flags)?;
            engine.invalidate(&project, &id)
        }
        "traces" => engine.traces(&TracesOptions {
            project,
            action: flags.action.unwrap_or_else(|| "list".into()),
            generation: flags.generation,
            trace_path: flags.trace_path,
            skill: flags.skill,
        }),
        "compile" => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| HarnessError::unavailable(format!("Failed to start runtime: {e}")))?;
            rt.block_on(engine.compile(&CompileOptions {
                project,
                generation: flags.generation,
                skill: flags.skill,
                trace_path: flags.trace_path,
                candidate_path: flags.candidate_path,
                provider: flags.provider,
                model: flags.model,
                round_dry_run: flags.dry_run,
            }))
        }
        "requalify" => {
            let id = require_capsule_id(&flags)?;
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| HarnessError::unavailable(format!("Failed to start runtime: {e}")))?;
            rt.block_on(engine.requalify(&project, &id))
        }
        _ => unreachable!(),
    }
}
