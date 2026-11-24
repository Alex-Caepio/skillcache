use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tempfile::TempDir;

fn engine_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_skillcache-engine"))
}

fn write_sample_hybrid_package(
    pkg_dir: &Path,
    examples_file: &Path,
    skill_dir: &Path,
    ws_rel_skill: &str,
) {
    let md_path = skill_dir.join("SKILL.md");
    std::fs::create_dir_all(skill_dir).unwrap();
    std::fs::write(
        &md_path,
        "---\nname: hello-skill\ndescription: Greet cleanly\n---\nRun python greeting.\n",
    )
    .unwrap();
    let tree = skillcache::core::fingerprint::tree_sha256(skill_dir).unwrap();

    let capsule_json = json!({
        "schema_version": skillcache::core::SCHEMA_VERSION,
        "form": "hybrid",
        "source": {
            "skills": [{
                "name": "hello-skill",
                "path": ws_rel_skill,
                "tree_sha256": tree.sha256,
            }]
        },
        "mu": {
            "name": "hello-skill",
            "description": "Deterministic python greeter capsule"
        },
        "capabilities": {
            "network": "none",
            "runtimes": ["python3"]
        },
        "bindings": [{
            "name": "who",
            "type": "string",
            "origin": "request",
            "description": "Name to greet"
        }],
        "programs": [{
            "id": "greet",
            "entry": ["python3", "programs/greet/main.py"],
            "inputs": ["who"],
            "outputs": [],
            "check": ["python3", "programs/greet/main.py", "--check"],
            "timeout_seconds": 15,
            "read_only": true,
            "description": "Format deterministic greeting JSON"
        }],
        "steps": [{
            "kind": "program",
            "program": "greet"
        }]
    });
    let main_py = r#"import json, os, sys
if "--check" in sys.argv:
    print(json.dumps({"status": "complete", "result": {}}))
    sys.exit(0)
payload = json.loads(os.environ.get("SKILLCACHE_INPUT") or "{}")
who = payload.get("who", "world")
print(json.dumps({"status": "complete", "result": {"greeting": f"hello {who}"}}))
"#;
    let parsed = skillcache::core::SkillCapsule::from_value(&capsule_json).unwrap();
    let mut files = skillcache::core::package::Files::new();
    files.insert("programs/greet/main.py", main_py.as_bytes().to_vec());
    let examples = skillcache::core::Examples::from_value(&json!({
        "bindings": {"who": "alice"},
        "steps": {"0": {"result": {"greeting": "hello alice"}}}
    }))
    .unwrap();
    skillcache::core::package::write_warm(pkg_dir, &parsed, None, &files).unwrap();
    skillcache::core::package::write_examples(examples_file, &examples).unwrap();
}

#[test]
fn multicall_argv0_skillcache_run_and_skillcache_tool_work() {
    let dir = TempDir::new().unwrap();
    let ws = dir.path().join("workspace");
    let pkg_dir = dir.path().join("pkg");
    let bin_dir = dir.path().join("bin");
    let state_file = dir.path().join("run-state.json");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&bin_dir).unwrap();

    let examples_file = dir.path().join("examples.json");
    write_sample_hybrid_package(
        &pkg_dir,
        &examples_file,
        &ws.join(".claude/skills/hello-skill"),
        ".claude/skills/hello-skill/SKILL.md",
    );

    let run_link = bin_dir.join("skillcache-run");
    let tool_link = bin_dir.join("skillcache-tool");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(engine_bin(), &run_link).unwrap();
        std::os::unix::fs::symlink(engine_bin(), &tool_link).unwrap();
    }

    // 1. skillcache-tool --check
    let check_out = Command::new(&tool_link)
        .args(["greet", "--check"])
        .current_dir(&ws)
        .env("SKILLCACHE_PACKAGE", &pkg_dir)
        .env("SKILLCACHE_WORKSPACE", &ws)
        .output()
        .unwrap();
    assert!(
        check_out.status.success(),
        "skillcache-tool --check failed: stdout={} stderr={}",
        String::from_utf8_lossy(&check_out.stdout),
        String::from_utf8_lossy(&check_out.stderr)
    );
    let check_json: Value = serde_json::from_slice(&check_out.stdout).unwrap();
    assert_eq!(check_json["status"], "complete");

    // 2. skillcache-run run '{"who":"codex"}'
    let run_out = Command::new(&run_link)
        .args(["run", r#"{"who":"codex"}"#])
        .current_dir(&ws)
        .env("SKILLCACHE_PACKAGE", &pkg_dir)
        .env("SKILLCACHE_WORKSPACE", &ws)
        .env("SKILLCACHE_STATE_FILE", &state_file)
        .output()
        .unwrap();
    assert!(
        run_out.status.success(),
        "skillcache-run failed: stdout={} stderr={}",
        String::from_utf8_lossy(&run_out.stdout),
        String::from_utf8_lossy(&run_out.stderr)
    );
    let run_json: Value = serde_json::from_slice(&run_out.stdout).unwrap();
    assert_eq!(run_json["status"], "complete");
    assert_eq!(run_json["result"]["greet"]["greeting"], "hello codex");

    // 3. Disambiguated `skillcache-engine status` in executor env vs `--project` mgmt mode
    let exec_status = Command::new(engine_bin())
        .arg("status")
        .current_dir(&ws)
        .env("SKILLCACHE_PACKAGE", &pkg_dir)
        .env("SKILLCACHE_WORKSPACE", &ws)
        .env("SKILLCACHE_STATE_FILE", &state_file)
        .output()
        .unwrap();
    assert!(exec_status.status.success());
    let exec_status_json: Value = serde_json::from_slice(&exec_status.stdout).unwrap();
    assert_eq!(exec_status_json["status"], "status");

    let state_dir = dir.path().join("mgmt-state");
    let mgmt_status = Command::new(engine_bin())
        .args([
            "status",
            "--project",
            ws.to_str().unwrap(),
            "--state-dir",
            state_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(mgmt_status.status.success());
    let mgmt_json: Value = serde_json::from_slice(&mgmt_status.stdout).unwrap();
    assert!(mgmt_json["project_id"].is_string());
    assert!(mgmt_json["providers"].is_array());
    skillcache::core::package::release(dir.path());
}

fn mcp_rpc(
    stdin: &mut impl Write,
    reader: &mut BufReader<impl std::io::Read>,
    req: Value,
) -> Value {
    let line = serde_json::to_string(&req).unwrap();
    writeln!(stdin, "{line}").unwrap();
    stdin.flush().unwrap();
    let mut resp_line = String::new();
    let n = reader.read_line(&mut resp_line).unwrap();
    assert!(n > 0, "MCP server closed stdout unexpectedly");
    serde_json::from_str(resp_line.trim()).unwrap()
}

#[test]
fn mcp_exposes_9_tools_enforces_plugin_cwd_guard_and_runs_lifecycle() {
    let dir = TempDir::new().unwrap();
    // Simulate a marketplace repo containing .git AND a packaged plugin subdir
    let parent_repo = dir.path().join("marketplace");
    let plugin_dir = parent_repo.join("plugins/skillcache");
    std::fs::create_dir_all(parent_repo.join(".git")).unwrap();
    std::fs::create_dir_all(plugin_dir.join(".codex-plugin")).unwrap();
    std::fs::create_dir_all(plugin_dir.join(".claude-plugin")).unwrap();
    std::fs::create_dir_all(plugin_dir.join("bin")).unwrap();
    std::fs::write(plugin_dir.join(".codex-plugin/plugin.json"), "{}").unwrap();
    std::fs::write(plugin_dir.join(".claude-plugin/plugin.json"), "{}").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(engine_bin(), plugin_dir.join("bin/skillcache-engine")).unwrap();

    let state_dir = dir.path().join("state");
    let ws = dir.path().join("target-repo");
    let pkg_dir = dir.path().join("valid-pkg");
    let examples_file = dir.path().join("examples.json");
    std::fs::create_dir_all(&ws).unwrap();
    write_sample_hybrid_package(
        &pkg_dir,
        &examples_file,
        &ws.join(".claude/skills/hello-skill"),
        ".claude/skills/hello-skill/SKILL.md",
    );

    let mut child = Command::new(engine_bin())
        .arg("mcp")
        .current_dir(&plugin_dir)
        .env("SKILLCACHE_STATE_DIR", &state_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    // 1. Initialize handshake (2025-06-18)
    let init = mcp_rpc(
        &mut stdin,
        &mut reader,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test-client", "version": "1.0"}
            }
        }),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .unwrap();
    stdin.flush().unwrap();

    // 2. Verify exact 9 tools
    let listed = mcp_rpc(
        &mut stdin,
        &mut reader,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    let tools = listed["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(
        names,
        vec![
            "skillcache_status",
            "skillcache_capsules_list",
            "skillcache_capsule_inspect",
            "skillcache_gate1",
            "skillcache_compile",
            "skillcache_requalify",
            "skillcache_retire",
            "skillcache_invalidate",
            "skillcache_traces",
        ]
    );
    for t in tools {
        assert_eq!(t["inputSchema"]["additionalProperties"], false);
    }

    // 3. plugin-cwd guard blocks omitted / "." / "./." project inside packaged plugin dir with ancestor .git
    for args in [json!({}), json!({"project": "."}), json!({"project": "./."})] {
        let refused = mcp_rpc(
            &mut stdin,
            &mut reader,
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {"name": "skillcache_status", "arguments": args}
            }),
        );
        assert_eq!(refused["result"]["isError"], true);
        assert_eq!(
            refused["result"]["structuredContent"]["code"],
            "plugin-cwd"
        );
    }
    let gate1_refused = mcp_rpc(
        &mut stdin,
        &mut reader,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "skillcache_gate1",
                "arguments": {"package_dir": "."}
            }
        }),
    );
    assert_eq!(gate1_refused["result"]["isError"], true);
    assert_eq!(
        gate1_refused["result"]["structuredContent"]["code"],
        "plugin-cwd"
    );

    // 4. Explicit project status succeeds
    let ok_status = mcp_rpc(
        &mut stdin,
        &mut reader,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "skillcache_status",
                "arguments": {"project": ws.to_str().unwrap()}
            }
        }),
    );
    assert_eq!(ok_status["result"]["isError"], false);
    assert!(ok_status["result"]["structuredContent"]["project_id"].is_string());

    // 5. Explicit Gate 1 succeeds on valid package
    let gate1_ok = mcp_rpc(
        &mut stdin,
        &mut reader,
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "skillcache_gate1",
                "arguments": {
                    "package_dir": pkg_dir.to_str().unwrap(),
                    "examples_path": examples_file.to_str().unwrap()
                }
            }
        }),
    );
    assert_eq!(gate1_ok["result"]["isError"], false);
    assert_eq!(gate1_ok["result"]["structuredContent"]["passed"], true);

    // 6. Trace ingest + list + show
    let trace_file = dir.path().join("trace.json");
    std::fs::write(
        &trace_file,
        json!({
            "generation": "gen-test-01",
            "provider": "claude",
            "request": "greet alice cleanly",
            "skill": "hello-skill",
            "observed_output": "{\"greeting\":\"hello alice\"}",
            "steps": [{
                "kind": "tool",
                "seq": 1,
                "tool": "Bash",
                "input": {"command": "python3 -c 'print(1)'"},
                "output": "1"
            }]
        })
        .to_string(),
    )
    .unwrap();

    let ingested = mcp_rpc(
        &mut stdin,
        &mut reader,
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "skillcache_traces",
                "arguments": {
                    "project": ws.to_str().unwrap(),
                    "action": "ingest",
                    "trace_path": trace_file.to_str().unwrap()
                }
            }
        }),
    );
    assert_eq!(ingested["result"]["isError"], false, "{ingested}");
    assert_eq!(
        ingested["result"]["structuredContent"]["replayable"],
        true
    );

    let listed_traces = mcp_rpc(
        &mut stdin,
        &mut reader,
        json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": {
                "name": "skillcache_traces",
                "arguments": {"project": ws.to_str().unwrap(), "action": "list"}
            }
        }),
    );
    assert_eq!(listed_traces["result"]["isError"], false);
    assert_eq!(listed_traces["result"]["structuredContent"]["count"], 1);

    // 7. Compile via direct candidate through real Gate 1 + Gate 2 + Gate 3 (if sandbox is available)
    if skillcache::host::sandbox::available() {
        let candidate_file = dir.path().join("candidate.json");
        let cap_bytes =
            std::fs::read_to_string(pkg_dir.join("capsule.json")).unwrap();
        let ex_bytes =
            std::fs::read_to_string(&examples_file).unwrap();
        let py_bytes =
            std::fs::read_to_string(pkg_dir.join("programs/greet/main.py")).unwrap();
        let cap_val: Value = serde_json::from_str(&cap_bytes).unwrap();
        let ex_val: Value = serde_json::from_str(&ex_bytes).unwrap();
        std::fs::write(
            &candidate_file,
            json!({
                "form": "hybrid",
                "capsule": cap_val,
                "files": {"programs/greet/main.py": py_bytes},
                "examples": ex_val,
                "observed_output": "{\"greeting\":\"hello alice\"}"
            })
            .to_string(),
        )
        .unwrap();

        let compiled = mcp_rpc(
            &mut stdin,
            &mut reader,
            json!({
                "jsonrpc": "2.0",
                "id": 9,
                "method": "tools/call",
                "params": {
                    "name": "skillcache_compile",
                    "arguments": {
                        "project": ws.to_str().unwrap(),
                        "candidate_path": candidate_file.to_str().unwrap()
                    }
                }
            }),
        );
        assert_eq!(compiled["result"]["isError"], false, "{compiled}");
        assert_eq!(
            compiled["result"]["structuredContent"]["passed"],
            true,
            "{compiled}"
        );
        assert_eq!(
            compiled["result"]["structuredContent"]["capsule"]["status"],
            "active"
        );
        let capsule_id = compiled["result"]["structuredContent"]["capsule_id"]
            .as_str()
            .unwrap()
            .to_owned();

        // list -> inspect -> invalidate -> requalify -> retire
        let cap_list = mcp_rpc(
            &mut stdin,
            &mut reader,
            json!({
                "jsonrpc": "2.0",
                "id": 10,
                "method": "tools/call",
                "params": {
                    "name": "skillcache_capsules_list",
                    "arguments": {"project": ws.to_str().unwrap()}
                }
            }),
        );
        assert_eq!(cap_list["result"]["isError"], false);
        assert_eq!(cap_list["result"]["structuredContent"]["counts"]["active"], 1);

        let inspected = mcp_rpc(
            &mut stdin,
            &mut reader,
            json!({
                "jsonrpc": "2.0",
                "id": 11,
                "method": "tools/call",
                "params": {
                    "name": "skillcache_capsule_inspect",
                    "arguments": {"project": ws.to_str().unwrap(), "capsule_id": capsule_id}
                }
            }),
        );
        assert_eq!(inspected["result"]["isError"], false);
        assert_eq!(
            inspected["result"]["structuredContent"]["capsule_id"],
            capsule_id
        );

        let invalidated = mcp_rpc(
            &mut stdin,
            &mut reader,
            json!({
                "jsonrpc": "2.0",
                "id": 12,
                "method": "tools/call",
                "params": {
                    "name": "skillcache_invalidate",
                    "arguments": {"project": ws.to_str().unwrap(), "capsule_id": capsule_id}
                }
            }),
        );
        assert_eq!(invalidated["result"]["isError"], false);
        assert_eq!(invalidated["result"]["structuredContent"]["status"], "stale");

        let requalified = mcp_rpc(
            &mut stdin,
            &mut reader,
            json!({
                "jsonrpc": "2.0",
                "id": 13,
                "method": "tools/call",
                "params": {
                    "name": "skillcache_requalify",
                    "arguments": {"project": ws.to_str().unwrap(), "capsule_id": capsule_id}
                }
            }),
        );
        assert_eq!(requalified["result"]["isError"], false, "{requalified}");
        assert_eq!(requalified["result"]["structuredContent"]["passed"], true);
        assert_eq!(
            requalified["result"]["structuredContent"]["capsule"]["status"],
            "active"
        );

        let retired = mcp_rpc(
            &mut stdin,
            &mut reader,
            json!({
                "jsonrpc": "2.0",
                "id": 14,
                "method": "tools/call",
                "params": {
                    "name": "skillcache_retire",
                    "arguments": {
                        "project": ws.to_str().unwrap(),
                        "capsule_id": capsule_id,
                        "reason": "superseded in test"
                    }
                }
            }),
        );
        assert_eq!(retired["result"]["isError"], false);
        assert_eq!(retired["result"]["structuredContent"]["status"], "retired");
    }

    drop(stdin);
    let exit = child.wait().unwrap();
    assert!(exit.success());
    skillcache::core::package::release(dir.path());
}

#[test]
fn mcp_enforces_plugin_cwd_guard_in_antigravity_gemini_cache_and_manifests() {
    let dir = TempDir::new().unwrap();
    let fake_gemini_ext = dir.path().join(".gemini/config/plugins/skillcache");
    std::fs::create_dir_all(&fake_gemini_ext).unwrap();
    std::fs::write(fake_gemini_ext.join("plugin.json"), "{}").unwrap();
    std::fs::write(fake_gemini_ext.join("mcp_config.json"), "{}").unwrap();

    let agy_standalone = dir.path().join("agy-ext-root");
    std::fs::create_dir_all(agy_standalone.join(".antigravity-plugin")).unwrap();
    std::fs::create_dir_all(agy_standalone.join("bin")).unwrap();
    std::fs::write(agy_standalone.join(".antigravity-plugin/plugin.json"), "{}").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        engine_bin(),
        agy_standalone.join("bin/skillcache-engine"),
    )
    .unwrap();

    let state_dir = dir.path().join("state");
    let ws = dir.path().join("target-repo");
    std::fs::create_dir_all(&ws).unwrap();

    for cwd in [&fake_gemini_ext, &agy_standalone] {
        let mut child = Command::new(engine_bin())
            .arg("mcp")
            .current_dir(cwd)
            .env("SKILLCACHE_STATE_DIR", &state_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let mut stdin = child.stdin.take().unwrap();
        let mut reader = BufReader::new(child.stdout.take().unwrap());

        let init = mcp_rpc(
            &mut stdin,
            &mut reader,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "antigravity-test", "version": "1.0"}
                }
            }),
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        )
        .unwrap();
        stdin.flush().unwrap();

        for args in [json!({}), json!({"project": "."}), json!({"project": "./."})] {
            let refused = mcp_rpc(
                &mut stdin,
                &mut reader,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/call",
                    "params": {"name": "skillcache_status", "arguments": args}
                }),
            );
            assert_eq!(refused["result"]["isError"], true, "{refused}");
            assert_eq!(
                refused["result"]["structuredContent"]["code"],
                "plugin-cwd",
                "{refused}"
            );
        }

        let gate1_refused = mcp_rpc(
            &mut stdin,
            &mut reader,
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "skillcache_gate1",
                    "arguments": {"package_dir": "."}
                }
            }),
        );
        assert_eq!(gate1_refused["result"]["isError"], true);
        assert_eq!(
            gate1_refused["result"]["structuredContent"]["code"],
            "plugin-cwd"
        );

        let ok_status = mcp_rpc(
            &mut stdin,
            &mut reader,
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "skillcache_status",
                    "arguments": {"project": ws.to_str().unwrap()}
                }
            }),
        );
        assert_eq!(ok_status["result"]["isError"], false, "{ok_status}");
        assert!(ok_status["result"]["structuredContent"]["project_id"].is_string());

        drop(stdin);
        let exit = child.wait().unwrap();
        assert!(exit.success());
    }

    skillcache::core::package::release(dir.path());
}
