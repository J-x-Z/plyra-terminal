// SPDX-License-Identifier: GPL-3.0-only
#![cfg(feature = "owned-fixture")]
use mcp_client::{Cancellation, Client, ServerConfig, TransportConfig};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn config(args: &[&str]) -> ServerConfig {
    ServerConfig {
        id: "owned-fixture".into(),
        name: "Owned fixture".into(),
        transport: TransportConfig::Stdio {
            program: env!("CARGO_BIN_EXE_glass-owned-mcp-fixture").into(),
            args: args.iter().map(|arg| (*arg).into()).collect(),
            env_refs: BTreeMap::new(),
            cwd: None,
        },
        connect_timeout_ms: 10_000,
        request_timeout_ms: 1500,
        shutdown_timeout_ms: 300,
        ..ServerConfig::default()
    }
}
fn call(client: &Client, name: &str, arguments: Value) -> Value {
    let prepared = client.prepare_tool_call(name, &arguments).unwrap();
    client.call_tool(&prepared.ticket).unwrap().data
}
fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap()
}

fn exercise(client: &Client) {
    assert!(client.info().connected);
    assert_eq!(client.info().server_name, "glass-owned-fixture");
    assert_eq!(client.list_tools().unwrap().len(), 4);
    assert_eq!(client.list_resources().unwrap()[0].uri, "fixture://note");
    assert_eq!(client.list_prompts().unwrap()[0].name, "greet");
    assert!(client
        .prepare_tool_call("unadvertised", &json!({}))
        .is_err());
    assert!(client.prepare_tool_call("echo", &json!([])).is_err());
    let pending = client
        .prepare_tool_call("echo", &json!({"message":"owned hello"}))
        .unwrap();
    assert_eq!(pending.arguments["message"], "owned hello");
    assert!(text(&client.call_tool(&pending.ticket).unwrap().data).contains("owned hello"));
    assert!(client.call_tool(&pending.ticket).is_err());
    let pending = client.prepare_tool_call("echo", &json!({})).unwrap();
    client.revoke_pending_calls();
    assert!(client.call_tool(&pending.ticket).is_err());
    assert_eq!(
        client.read_resource("fixture://note").unwrap().data["contents"][0]["text"],
        "owned resource"
    );
    assert_eq!(
        client
            .get_prompt("greet", &json!({"name":"Glass"}))
            .unwrap()
            .data["messages"][0]["content"]["text"],
        "Hello Glass"
    );
    assert!(client.get_prompt("greet", &json!({"name":123})).is_err());
}

#[test]
fn modern_and_legacy_stdio_protocol_and_confirmation() {
    for args in [vec![], vec!["--legacy"]] {
        let client = Client::connect(config(&args)).unwrap();
        if args.is_empty() {
            assert_eq!(client.info().protocol_version, "2026-07-28");
        } else {
            assert_eq!(client.info().protocol_version, "2025-11-25");
        }
        exercise(&client);
        let identity: Value =
            serde_json::from_str(text(&call(&client, "identity", json!({})))).unwrap();
        let pid = identity["pid"].as_u64().unwrap() as u32;
        let other = client.clone();
        let disconnect = std::thread::spawn(move || other.disconnect());
        client.disconnect().unwrap();
        disconnect.join().unwrap().unwrap();
        assert!(!client.info().connected);
        wait_gone(pid);
    }
}

#[test]
fn timeout_cancel_limits_and_final_drop() {
    let mut options = config(&[]);
    options.request_timeout_ms = 250;
    options.max_output_bytes = 2048;
    let client = Client::connect(options).unwrap();
    client.list_tools().unwrap();
    let pid: Value = serde_json::from_str(text(&call(&client, "identity", json!({})))).unwrap();
    let ticket = client.prepare_tool_call("slow", &json!({})).unwrap();
    let started = Instant::now();
    assert!(client
        .call_tool(&ticket.ticket)
        .unwrap_err()
        .to_string()
        .contains("timed out"));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(text(&call(&client, "echo", json!({"after":"timeout"}))).contains("timeout"));
    let ticket = client.prepare_tool_call("slow", &json!({})).unwrap();
    let worker_client = client.clone();
    let worker = std::thread::spawn(move || worker_client.call_tool(&ticket.ticket));
    std::thread::sleep(Duration::from_millis(60));
    client.cancel_active_requests();
    assert!(worker.join().unwrap().is_err());
    assert!(text(&call(&client, "echo", json!({"after":"cancel"}))).contains("cancel"));
    let ticket = client.prepare_tool_call("large", &json!({})).unwrap();
    assert!(client
        .call_tool(&ticket.ticket)
        .unwrap_err()
        .to_string()
        .contains("output byte limit"));
    drop(client);
    wait_gone(pid["pid"].as_u64().unwrap() as u32);
}

#[test]
fn explicit_environment_references_and_cancel_connect() {
    // Dummy values only. This fixture never reads production credentials.
    std::env::set_var("GLASS_MCP_REFERENCE_CANARY", "dummy-explicit-value");
    std::env::set_var("GLASS_MCP_UNREFERENCED_CANARY", "dummy-must-not-inherit");
    let mut options = config(&[]);
    if let TransportConfig::Stdio { env_refs, cwd, .. } = &mut options.transport {
        env_refs.insert("FIXTURE_VALUE".into(), "GLASS_MCP_REFERENCE_CANARY".into());
        *cwd = Some(std::env::temp_dir().to_string_lossy().into());
    }
    let client = Client::connect(options).unwrap();
    client.list_tools().unwrap();
    let identity: Value =
        serde_json::from_str(text(&call(&client, "identity", json!({})))).unwrap();
    assert_eq!(identity["explicit"], "dummy-explicit-value");
    assert_eq!(identity["inherited"], Value::Null);
    assert_eq!(
        std::path::Path::new(identity["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        std::env::temp_dir().canonicalize().unwrap()
    );
    client.disconnect().unwrap();
    std::env::remove_var("GLASS_MCP_REFERENCE_CANARY");
    std::env::remove_var("GLASS_MCP_UNREFERENCED_CANARY");
    let cancellation = Cancellation::default();
    let worker_cancel = cancellation.clone();
    let pid_file =
        std::env::temp_dir().join(format!("glass-mcp-connect-{}.pid", std::process::id()));
    let _ = std::fs::remove_file(&pid_file);
    let worker_path = pid_file.clone();
    let started = Instant::now();
    let worker = std::thread::spawn(move || {
        Client::connect_with_cancel(
            config(&[
                "--startup-delay",
                "--pid-file",
                worker_path.to_str().unwrap(),
            ]),
            worker_cancel,
        )
    });
    while !pid_file.exists() {
        assert!(started.elapsed() < Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(10));
    }
    cancellation.cancel_and_wait().unwrap();
    assert!(worker.join().unwrap().is_err());
    assert!(started.elapsed() < Duration::from_secs(2));
    let pid = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
    wait_gone(pid);
    std::fs::remove_file(pid_file).unwrap();
}

struct HttpFixture(Child);
impl Drop for HttpFixture {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn streamable_http_owned_loopback_protocol() {
    let mut command = Command::new(env!("CARGO_BIN_EXE_glass-owned-mcp-fixture"));
    command
        .args(["--http", "--require-auth"])
        .env_clear()
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Winsock and the Windows runtime need the OS directory even in this
    // isolated fixture. Production stdio transport retains the same variable.
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", root);
    }
    let mut server = HttpFixture(command.spawn().unwrap());
    let mut endpoint = String::new();
    BufReader::new(server.0.stdout.take().unwrap())
        .read_line(&mut endpoint)
        .unwrap();
    assert!(endpoint.starts_with("http://127.0.0.1:"));
    let mut options = config(&[]);
    options.transport = TransportConfig::StreamableHttp {
        url: endpoint.trim().into(),
        bearer_token_env: None,
    };
    assert!(Client::connect(options.clone()).is_err());
    std::env::set_var("GLASS_MCP_OWNED_HTTP_TOKEN", "dummy-owned-token");
    if let TransportConfig::StreamableHttp {
        bearer_token_env, ..
    } = &mut options.transport
    {
        *bearer_token_env = Some("GLASS_MCP_OWNED_HTTP_TOKEN".into());
    }
    let client = Client::connect(options).unwrap();
    assert_eq!(client.info().transport, "streamable_http");
    exercise(&client);
    client.disconnect().unwrap();
    std::env::remove_var("GLASS_MCP_OWNED_HTTP_TOKEN");
}

#[test]
fn configuration_is_versionable_validated_and_redacted() {
    let mut options = config(&["sensitive-test-argument"]);
    options.validate().unwrap();
    let serialized = serde_json::to_value(&options).unwrap();
    assert_eq!(serialized["transport"]["kind"], "stdio");
    let restored: ServerConfig = serde_json::from_value(serialized).unwrap();
    restored.validate().unwrap();
    assert!(!format!("{restored:?}").contains("sensitive-test-argument"));
    options.transport = TransportConfig::StreamableHttp {
        url: "https://example.invalid/mcp?secret=dummy".into(),
        bearer_token_env: Some("MCP_TOKEN".into()),
    };
    assert!(!format!("{options:?}").contains("secret=dummy"));
    options.transport = TransportConfig::StreamableHttp {
        url: "https://username:secret@example.invalid/mcp".into(),
        bearer_token_env: None,
    };
    assert!(options.validate().is_err());
    assert!(
        serde_json::from_value::<ServerConfig>(json!({"id":"valid","unknown_field":1})).is_err()
    );
}

fn wait_gone(pid: u32) {
    #[cfg(unix)]
    {
        let deadline = Instant::now() + Duration::from_secs(5);
        while unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
            assert!(
                Instant::now() < deadline,
                "owned MCP server {pid} survived cleanup"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

// FVPROV1 10222 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiZDUyNGFiOWQwNTc5Nzc5N2UyMTA2MTE0MjM1OTM4MjNmYmNjNDQ2YThiMjYyN2EyYWU2M2VlMmY2MmIxN2YwYSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiZTdmODI0OTU3YTJiNDA4ZDQ4YjA2NmE5ZGY0ZWRmM2RiMWNiMDFmY2VjMjU2YjE4MWQ4MTUzN2JiMTM5YTlkZTdjZGRhOWFjNTMxOTZjOWRiZTU5ZGFlZTlkNDE1MTk2NmNmODE1YzU4ODZlYzA1Mzc4YjA0OTczYzJjNWRiMGMifV19XQ== FVPROV1-END
