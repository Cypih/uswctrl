// Integration tests for `uswctl mcp` over stdio, against the real router and a simulated XG 10.

mod common;

use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use common::{AGENT, StateEnv, state_build};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::TcpListener;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use uswctl::api::router_build;

// A real write on the simulated switch waits for the read-back before it ends in no_effect.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);
const PROTOCOL_VERSION: &str = "2025-11-25";

async fn server_start() -> (SocketAddr, StateEnv) {
    let env = state_build(false).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = router_build(env.state.clone());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (addr, env)
}

struct McpSession {
    // Held so that `kill_on_drop` stops the server at the end of the test.
    _child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl McpSession {
    fn session_spawn(addr: SocketAddr, owner: Option<&str>) -> McpSession {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_uswctl"));
        cmd.arg("mcp")
            .env("USWCTL_URL", format!("http://{addr}"))
            .env("USWCTL_TOKEN", AGENT)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        match owner {
            Some(owner) => cmd.env("USWCTL_OWNER", owner),
            None => cmd.env_remove("USWCTL_OWNER"),
        };
        let mut child = cmd.spawn().expect("start uswctl mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        McpSession {
            _child: child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    async fn message_send(&mut self, message: &Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    // Skips any message that is not the answer to `id`, such as a log notification.
    async fn answer_read(&mut self, id: u64) -> Value {
        loop {
            let line = tokio::time::timeout(ANSWER_TIMEOUT, self.stdout.next_line())
                .await
                .expect("an answer before the timeout")
                .unwrap()
                .expect("uswctl mcp closed its stdout");
            let message: Value =
                serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"));
            if message["id"] == json!(id) {
                return message;
            }
        }
    }

    async fn request_send(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.message_send(&request).await;
        self.answer_read(id).await
    }

    async fn session_initialize(&mut self) -> Value {
        let answer = self
            .request_send(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "uswctl-test", "version": "0" }
                }),
            )
            .await;
        self.message_send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await;
        answer
    }

    async fn tool_call(&mut self, name: &str, arguments: Value) -> Value {
        self.request_send(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
        .await
    }
}

async fn session_start(addr: SocketAddr, owner: Option<&str>) -> McpSession {
    let mut session = McpSession::session_spawn(addr, owner);
    session.session_initialize().await;
    session
}

fn result_text(answer: &Value) -> String {
    answer["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text content: {answer}"))
        .to_string()
}

fn result_is_error(answer: &Value) -> bool {
    answer["result"]["isError"].as_bool() == Some(true)
}

#[tokio::test]
async fn initialize_answers_with_tools() {
    let (addr, _env) = server_start().await;
    let mut session = McpSession::session_spawn(addr, Some("a"));
    let answer = session.session_initialize().await;
    let result = &answer["result"];
    assert_eq!(result["protocolVersion"], PROTOCOL_VERSION, "{answer}");
    assert!(result["capabilities"]["tools"].is_object(), "{answer}");
    assert_eq!(result["serverInfo"]["name"], "uswctl", "{answer}");
}

#[tokio::test]
async fn tools_list_has_every_tool_with_schema() {
    let (addr, _env) = server_start().await;
    let mut session = session_start(addr, Some("a")).await;
    let answer = session.request_send("tools/list", json!({})).await;
    let tools = answer["result"]["tools"].as_array().expect("a tool list");
    let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "events_get",
            "macs_get",
            "port_action",
            "port_get",
            "port_locate",
            "port_locate_off",
            "port_reserve",
            "port_set",
            "ports_list",
            "reservation_extend",
            "reservation_release",
            "reservations_list",
            "switches_list",
        ]
    );
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
        assert!(tool["description"].is_string(), "{tool}");
    }
    let tool = |name: &str| tools.iter().find(|t| t["name"] == name).unwrap().clone();
    let port_set = tool("port_set")["inputSchema"].clone();
    assert_eq!(port_set["required"], json!(["switch", "port"]));
    assert_eq!(
        port_set["properties"]["poe"]["enum"],
        json!(["auto", "off"])
    );
    assert_eq!(
        port_set["properties"]["edge"]["enum"],
        json!(["auto", "enabled", "disabled"])
    );
    assert_eq!(port_set["properties"]["dry_run"]["type"], "boolean");
    assert_eq!(port_set["properties"]["dry_run"]["default"], true);
    assert_eq!(port_set["properties"]["stp"]["type"], "boolean");
    assert_eq!(port_set["additionalProperties"], false);
    let port_action = tool("port_action")["inputSchema"].clone();
    assert_eq!(port_action["required"], json!(["switch", "port", "action"]));
    assert_eq!(
        port_action["properties"]["action"]["enum"],
        json!(["link_down", "link_up", "poe_off", "poe_on", "poe_cycle"])
    );
    assert_eq!(
        tool("port_reserve")["inputSchema"]["required"],
        json!(["switch", "port", "ttl_s"])
    );
    assert_eq!(
        tool("reservation_extend")["inputSchema"]["required"],
        json!(["id", "ttl_s"])
    );
    let port_locate = tool("port_locate")["inputSchema"].clone();
    assert_eq!(port_locate["required"], json!(["switch", "ports"]));
    assert_eq!(port_locate["properties"]["ports"]["type"], "array");
    assert_eq!(port_locate["properties"]["duration_s"]["minimum"], 10);
    assert_eq!(port_locate["properties"]["duration_s"]["maximum"], 3600);
    assert_eq!(port_locate["properties"]["duration_s"]["default"], 300);
    assert_eq!(
        tool("port_locate_off")["inputSchema"]["required"],
        json!(["switch"])
    );
    assert_eq!(port_locate["properties"]["owner"]["type"], "string");
    assert_eq!(
        tool("port_locate_off")["inputSchema"]["properties"]["owner"]["type"],
        "string"
    );
    assert_eq!(tool("port_locate")["annotations"]["destructiveHint"], false);
    assert_eq!(tool("switches_list")["annotations"]["readOnlyHint"], true);
    assert_eq!(tool("port_set")["annotations"]["readOnlyHint"], false);
}

#[tokio::test]
async fn ports_list_returns_json_for_xg10() {
    let (addr, _env) = server_start().await;
    let mut session = session_start(addr, None).await;
    let answer = session
        .tool_call("ports_list", json!({ "switch": "xg10" }))
        .await;
    assert!(!result_is_error(&answer), "{answer}");
    let text = result_text(&answer);
    let body: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
    let ports = body["ports"].as_array().expect("a port list");
    assert_eq!(ports.len(), 12, "{text}");
    let port3 = ports.iter().find(|p| p["idx"] == 3).unwrap();
    assert_eq!(port3["native_vlan"], 3, "{text}");
    assert_eq!(port3["tagged_vlans"], json!([]), "{text}");
    assert_eq!(port3["stp"], true, "{text}");
    assert_eq!(port3["reserved_by"], Value::Null, "{text}");
    let port7 = ports.iter().find(|p| p["idx"] == 7).unwrap();
    assert_eq!(port7["vlan_all"], true, "{text}");
}

#[tokio::test]
async fn port_get_returns_port_view() {
    let (addr, _env) = server_start().await;
    let mut session = session_start(addr, None).await;
    let answer = session
        .tool_call("port_get", json!({ "switch": "xg10", "port": 3 }))
        .await;
    assert!(!result_is_error(&answer), "{answer}");
    let body: Value = serde_json::from_str(&result_text(&answer)).unwrap();
    assert_eq!(body["idx"], 3, "{body}");
    assert!(body["settings"].is_object(), "{body}");
    assert!(body["live"].is_object(), "{body}");
}

#[tokio::test]
async fn port_set_without_dry_run_makes_no_write() {
    let (addr, env) = server_start().await;
    let mut session = session_start(addr, Some("a")).await;
    let puts_before = env.fake.file_puts_get().len();
    let answer = session
        .tool_call(
            "port_set",
            json!({ "switch": "xg10", "port": 3, "name": "DUT 3" }),
        )
        .await;
    assert!(!result_is_error(&answer), "{answer}");
    let body: Value = serde_json::from_str(&result_text(&answer)).unwrap();
    assert!(body["keys"].is_array(), "{body}");
    assert_eq!(env.fake.file_puts_get().len(), puts_before);
}

#[tokio::test]
async fn port_set_write_without_reservation_refused() {
    let (addr, env) = server_start().await;
    let mut session = session_start(addr, Some("a")).await;
    let puts_before = env.fake.file_puts_get().len();
    let answer = session
        .tool_call(
            "port_set",
            json!({ "switch": "xg10", "port": 3, "name": "DUT 3", "dry_run": false }),
        )
        .await;
    assert!(result_is_error(&answer), "{answer}");
    assert!(result_text(&answer).contains("reservation"), "{answer}");
    assert_eq!(env.fake.file_puts_get().len(), puts_before);
}

#[tokio::test]
async fn port_set_write_with_other_owner_reservation_refused() {
    let (addr, _env) = server_start().await;
    let mut session = session_start(addr, Some("a")).await;
    let reserved = session
        .tool_call(
            "port_reserve",
            json!({ "switch": "xg10", "port": 3, "ttl_s": 60, "owner": "b" }),
        )
        .await;
    assert!(!result_is_error(&reserved), "{reserved}");
    let answer = session
        .tool_call(
            "port_set",
            json!({ "switch": "xg10", "port": 3, "name": "DUT 3", "dry_run": false }),
        )
        .await;
    assert!(result_is_error(&answer), "{answer}");
    assert!(result_text(&answer).contains("reservation"), "{answer}");
}

// The simulated switch never changes its system.cfg, so a real write that passes every
// check ends in no_effect (422) at the read-back: that error proves the write reached it.
#[tokio::test]
async fn port_set_write_with_reservation_reaches_daemon() {
    let (addr, _env) = server_start().await;
    let mut session = session_start(addr, Some("a")).await;
    let reserved = session
        .tool_call(
            "port_reserve",
            json!({ "switch": "xg10", "port": 3, "ttl_s": 60 }),
        )
        .await;
    assert!(!result_is_error(&reserved), "{reserved}");
    let answer = session
        .tool_call(
            "port_set",
            json!({ "switch": "xg10", "port": 3, "name": "DUT 3", "dry_run": false }),
        )
        .await;
    assert!(result_is_error(&answer), "{answer}");
    let text = result_text(&answer);
    assert!(text.contains("no_effect"), "{text}");
    assert!(text.contains("422"), "{text}");
}

#[tokio::test]
async fn port_action_without_reservation_refused() {
    let (addr, env) = server_start().await;
    let mut session = session_start(addr, Some("a")).await;
    let scripts_before = env.fake.scripts_get().len();
    let answer = session
        .tool_call(
            "port_action",
            json!({ "switch": "xg10", "port": 1, "action": "link_down" }),
        )
        .await;
    assert!(result_is_error(&answer), "{answer}");
    assert!(result_text(&answer).contains("reservation"), "{answer}");
    let scripts = env.fake.scripts_get();
    assert!(
        scripts[scripts_before..]
            .iter()
            .all(|s| !s.contains("shutdown")),
        "{scripts:?}"
    );
}

#[tokio::test]
async fn port_action_with_reservation_runs() {
    let (addr, _env) = server_start().await;
    let mut session = session_start(addr, Some("a")).await;
    let reserved = session
        .tool_call(
            "port_reserve",
            json!({ "switch": "xg10", "port": 1, "ttl_s": 60 }),
        )
        .await;
    assert!(!result_is_error(&reserved), "{reserved}");
    let answer = session
        .tool_call(
            "port_action",
            json!({ "switch": "xg10", "port": 1, "action": "link_down" }),
        )
        .await;
    assert!(!result_is_error(&answer), "{answer}");
    let body: Value = serde_json::from_str(&result_text(&answer)).unwrap();
    assert_eq!(body["action"], "link_down", "{body}");
}

#[tokio::test]
async fn reservation_extend_and_release() {
    let (addr, _env) = server_start().await;
    let mut session = session_start(addr, Some("a")).await;
    let reserved = session
        .tool_call(
            "port_reserve",
            json!({ "switch": "xg10", "port": 4, "ttl_s": 60 }),
        )
        .await;
    let body: Value = serde_json::from_str(&result_text(&reserved)).unwrap();
    let id = body["id"].as_u64().unwrap();
    let extended = session
        .tool_call("reservation_extend", json!({ "id": id, "ttl_s": 120 }))
        .await;
    assert!(!result_is_error(&extended), "{extended}");
    let released = session
        .tool_call("reservation_release", json!({ "id": id }))
        .await;
    assert!(!result_is_error(&released), "{released}");
    let listed = session.tool_call("reservations_list", json!({})).await;
    assert_eq!(result_text(&listed), "[]", "{listed}");
}

#[tokio::test]
async fn write_tool_without_owner_refused() {
    let (addr, _env) = server_start().await;
    let mut session = session_start(addr, None).await;
    let answer = session
        .tool_call(
            "port_reserve",
            json!({ "switch": "xg10", "port": 3, "ttl_s": 60 }),
        )
        .await;
    assert!(result_is_error(&answer), "{answer}");
    assert!(result_text(&answer).contains("USWCTL_OWNER"), "{answer}");
    let listed = session.tool_call("reservations_list", json!({})).await;
    assert_eq!(result_text(&listed), "[]", "{listed}");
}

#[tokio::test]
async fn bad_argument_is_tool_error() {
    let (addr, _env) = server_start().await;
    let mut session = session_start(addr, Some("a")).await;
    let answer = session
        .tool_call(
            "port_set",
            json!({ "switch": "xg10", "port": 3, "poe": "on" }),
        )
        .await;
    assert!(result_is_error(&answer), "{answer}");
    assert!(result_text(&answer).contains("poe"), "{answer}");
}

#[tokio::test]
async fn unknown_tool_is_json_rpc_error() {
    let (addr, _env) = server_start().await;
    let mut session = session_start(addr, Some("a")).await;
    let answer = session.tool_call("switch_reboot", json!({})).await;
    assert!(answer.get("result").is_none(), "{answer}");
    assert_eq!(answer["error"]["code"], -32602, "{answer}");
}

#[tokio::test]
async fn dry_run_as_string_refused() {
    let (addr, env) = server_start().await;
    let mut session = session_start(addr, Some("a")).await;
    let puts_before = env.fake.file_puts_get().len();
    let answer = session
        .tool_call(
            "port_set",
            json!({ "switch": "xg10", "port": 3, "name": "DUT 3", "dry_run": "false" }),
        )
        .await;
    assert!(result_is_error(&answer), "{answer}");
    assert!(result_text(&answer).contains("dry_run"), "{answer}");
    assert_eq!(env.fake.file_puts_get().len(), puts_before);
}

#[tokio::test]
async fn unknown_argument_refused() {
    let (addr, env) = server_start().await;
    let mut session = session_start(addr, Some("a")).await;
    let scripts_before = env.fake.scripts_get().len();
    let answer = session
        .tool_call(
            "port_set",
            json!({ "switch": "xg10", "port": 3, "speed": 100 }),
        )
        .await;
    assert!(result_is_error(&answer), "{answer}");
    assert!(result_text(&answer).contains("speed"), "{answer}");
    assert_eq!(env.fake.scripts_get().len(), scripts_before);
}

#[tokio::test]
async fn port_as_string_refused() {
    let (addr, _env) = server_start().await;
    let mut session = session_start(addr, None).await;
    let answer = session
        .tool_call("port_get", json!({ "switch": "xg10", "port": "3" }))
        .await;
    assert!(result_is_error(&answer), "{answer}");
    assert!(result_text(&answer).contains("port"), "{answer}");
}

#[tokio::test]
async fn bad_switch_name_refused() {
    let (addr, _env) = server_start().await;
    let mut session = session_start(addr, None).await;
    for name in ["xg10/ports/5", "xg10?dry_run=0", "xg10#", ".", "..", ""] {
        let answer = session
            .tool_call("port_get", json!({ "switch": name, "port": 3 }))
            .await;
        assert!(result_is_error(&answer), "{name:?}: {answer}");
        assert!(
            result_text(&answer).contains("switch name"),
            "{name:?}: {answer}"
        );
    }
}

#[tokio::test]
async fn owner_argument_is_trimmed() {
    let (addr, _env) = server_start().await;
    let mut session = session_start(addr, Some("a")).await;
    let reserved = session
        .tool_call(
            "port_reserve",
            json!({ "switch": "xg10", "port": 3, "ttl_s": 60, "owner": " a " }),
        )
        .await;
    assert!(!result_is_error(&reserved), "{reserved}");
    let body: Value = serde_json::from_str(&result_text(&reserved)).unwrap();
    assert_eq!(body["owner"], "a", "{body}");
    let answer = session
        .tool_call(
            "port_set",
            json!({ "switch": "xg10", "port": 3, "name": "DUT 3", "dry_run": false }),
        )
        .await;
    let text = result_text(&answer);
    assert!(text.contains("no_effect"), "{text}");
}

// A locate needs no owner and no reservation, also on a protected port.
#[tokio::test]
async fn port_locate_and_off_without_owner() {
    let (addr, env) = server_start().await;
    let mut session = session_start(addr, None).await;
    let answer = session
        .tool_call(
            "port_locate",
            json!({ "switch": "xg10", "ports": [11, 1], "duration_s": 60, "owner": "session-a" }),
        )
        .await;
    assert!(!result_is_error(&answer), "{answer}");
    let body: Value = serde_json::from_str(&result_text(&answer)).unwrap();
    assert_eq!(body["ports"], json!([1, 11]), "{body}");
    assert_eq!(
        body["cli_lines"],
        json!(["syswrapper.sh set-port-locate 0 401"])
    );

    let answer = session
        .tool_call(
            "port_locate_off",
            json!({ "switch": "xg10", "owner": "session-b" }),
        )
        .await;
    assert!(!result_is_error(&answer), "{answer}");
    let scripts: Vec<String> = env
        .fake
        .scripts_get()
        .into_iter()
        .filter(|s| s.contains("port-locate"))
        .collect();
    assert_eq!(
        scripts,
        vec![
            "syswrapper.sh set-port-locate 0 401",
            "syswrapper.sh unset-port-locate"
        ]
    );
}

#[tokio::test]
async fn port_locate_bad_arguments_refused() {
    let (addr, env) = server_start().await;
    let mut session = session_start(addr, None).await;
    for arguments in [
        json!({ "switch": "xg10", "ports": "1" }),
        json!({ "switch": "xg10", "ports": [1, "2"] }),
        json!({ "switch": "xg10", "ports": [13] }),
        json!({ "switch": "xg10", "ports": [1], "duration_s": 5 }),
        json!({ "switch": "xg10", "ports": [1], "colour": "red" }),
    ] {
        let answer = session.tool_call("port_locate", arguments.clone()).await;
        assert!(result_is_error(&answer), "{arguments} -> {answer}");
    }
    assert!(
        env.fake
            .scripts_get()
            .iter()
            .all(|s| !s.contains("port-locate"))
    );
}
