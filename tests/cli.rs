// Integration tests for the command-line client: it runs as a real subprocess against a
// real HTTP server, the same router the daemon serves, behind a simulated XG 10.

mod common;

use std::net::SocketAddr;
use std::process::{Command, Output as ProcOutput, Stdio};

use common::{AGENT, StateEnv, state_build};
use serde_json::Value;
use tokio::net::TcpListener;
use uswctl::api::router_build;

// Keeps `_env` (its temp dir, fake transport and simulated device) alive for the life of
// the test: the server task borrows nothing, but the data dir it reads from must stay put.
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

// Runs the real `uswctl` binary against `addr`. Blocking `Command::output` would starve the
// single-threaded test runtime if run inline, so it runs on the blocking pool instead, which
// leaves the executor free to drive the spawned server and device worker.
async fn cli_run(
    addr: SocketAddr,
    token: &'static str,
    owner: Option<&str>,
    args: Vec<String>,
) -> ProcOutput {
    let owner = owner.map(str::to_string);
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_uswctl"));
        cmd.args(&args);
        cmd.env("USWCTL_URL", format!("http://{addr}"));
        cmd.env("USWCTL_TOKEN", token);
        match owner {
            Some(owner) => {
                cmd.env("USWCTL_OWNER", owner);
            }
            None => {
                cmd.env_remove("USWCTL_OWNER");
            }
        }
        cmd.output().expect("run the uswctl binary")
    })
    .await
    .unwrap()
}

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

// A clap parse error never reaches the HTTP layer, so these need no server and no token.
async fn cli_run_argv_only(args: Vec<String>) -> ProcOutput {
    tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_uswctl"))
            .args(&args)
            .env_remove("USWCTL_TOKEN")
            .env_remove("USWCTL_URL")
            .env_remove("USWCTL_OWNER")
            .output()
            .expect("run the uswctl binary")
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn ports_table_has_12_rows_xg() {
    let (addr, _env) = server_start().await;
    let output = cli_run(addr, AGENT, None, args(&["ports", "xg10"])).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    // The switch-wide line, one header row, and one row per port of the XG 10 (12 ports).
    assert_eq!(lines.len(), 14, "{text}");
    assert_eq!(
        lines[0],
        "STP version rstp, STP priority 32768, BPDU action flooding, VLANs 1,2,3,4,5,6,7,8"
    );
    let header: Vec<&str> = lines[1].split_whitespace().collect();
    assert_eq!(
        header,
        vec![
            "IDX",
            "NAME",
            "UP",
            "SPEED",
            "NATIVE",
            "TAGGED",
            "POE",
            "STP",
            "BPDU-FILTER",
            "ENABLED",
            "PROTECTED"
        ]
    );
    let column = |name: &str| header.iter().position(|h| *h == name).unwrap();
    let row = |idx: &str| {
        lines[2..]
            .iter()
            .map(|l| l.split_whitespace().collect::<Vec<_>>())
            .find(|cells| cells[0] == idx)
            .unwrap()
    };
    // The XG 10 fixture: port 1 has native VLAN 4, port 3 native VLAN 3 and no tagged
    // VLAN, and port 7 has no membership lines; port names hold one space each.
    assert_eq!(row("3")[column("NATIVE") + 1], "3", "{text}");
    assert_eq!(row("3")[column("TAGGED") + 1], "none", "{text}");
    assert_eq!(row("7")[column("TAGGED") + 1], "all", "{text}");
    assert_eq!(row("3")[column("STP") + 1], "true", "{text}");
    assert_eq!(row("3")[column("BPDU-FILTER") + 1], "false", "{text}");
}

// A write on a real switch takes longer than the 10 s that a connect may take, so a slow
// answer to a PATCH must still arrive.
#[tokio::test]
async fn slow_write_answer_arrives() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = stream.read(&mut buf).await.unwrap();
            request.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&request);
            if n == 0 || (text.contains("\r\n\r\n") && text.ends_with('}')) {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(12)).await;
        let body = r#"{"keys":[],"overlay_changed":false,"cli_lines":[],"readback_ok":true}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
connection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let output = cli_run(
        addr,
        AGENT,
        Some("a"),
        args(&["port-set", "xg10", "3", "--name", "DUT 3"]),
    )
    .await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn link_down_exit_0() {
    let (addr, _env) = server_start().await;
    let output = cli_run(
        addr,
        AGENT,
        Some("tester"),
        args(&["link", "xg10", "1", "down"]),
    )
    .await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn reserved_exit_2() {
    let (addr, _env) = server_start().await;
    let reserved = cli_run(
        addr,
        AGENT,
        Some("a"),
        args(&["reserve", "xg10", "3", "--ttl", "60"]),
    )
    .await;
    assert_eq!(
        reserved.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&reserved.stderr)
    );
    let output = cli_run(addr, AGENT, Some("b"), args(&["link", "xg10", "3", "down"])).await;
    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn no_effect_exit_3() {
    let (addr, _env) = server_start().await;
    // The simulated XG 10's system.cfg never changes, so a real (non-dry-run) write always
    // ends in NoEffect (422) at the read-back: see `common::Sim`.
    let output = cli_run(
        addr,
        AGENT,
        Some("a"),
        args(&["port-set", "xg10", "3", "--name", "DUT 3"]),
    )
    .await;
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// clap's own exit code for a parse error (2) would be indistinguishable from the brief's
// exit code 2 for a server-side refusal (401, 403, 409, 423); a bad argument must not use it.
#[tokio::test]
async fn bad_port_exit_1() {
    let output = cli_run_argv_only(args(&["link", "xg10", "abc", "down"])).await;
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn no_subcommand_exit_1() {
    let output = cli_run_argv_only(args(&[])).await;
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn json_flag_prints_json() {
    let (addr, _env) = server_start().await;
    let output = cli_run(addr, AGENT, None, args(&["--json", "ports", "xg10"])).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let value: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
    assert_eq!(value["ports"].as_array().unwrap().len(), 12, "{text}");
}

fn output_describe(output: &ProcOutput) -> String {
    format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[tokio::test]
async fn locate_and_locate_off_exit_0() {
    let (addr, env) = server_start().await;
    let output = cli_run(
        addr,
        AGENT,
        None,
        args(&["locate", "xg10", "1,3-4,11", "--for", "5m"]),
    )
    .await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        output_describe(&output)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("ports: 1,3,4,11"), "{text}");
    assert!(
        text.contains("syswrapper.sh set-port-locate 0 40d"),
        "{text}"
    );

    let output = cli_run(addr, AGENT, None, args(&["ports", "xg10"])).await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        output_describe(&output)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines[1].starts_with("Locate: ports 1,3,4,11 until "),
        "{text}"
    );
    assert!(lines[2].starts_with("IDX"), "{text}");

    let output = cli_run(addr, AGENT, None, args(&["switches"])).await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        output_describe(&output)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("LOCATE"), "{text}");
    assert!(text.contains("1,3,4,11 until "), "{text}");

    let output = cli_run(addr, AGENT, None, args(&["locate-off", "xg10"])).await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        output_describe(&output)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("syswrapper.sh unset-port-locate"), "{text}");
    let scripts: Vec<String> = env
        .fake
        .scripts_get()
        .into_iter()
        .filter(|s| s.contains("port-locate"))
        .collect();
    assert_eq!(
        scripts,
        vec![
            "syswrapper.sh set-port-locate 0 40d",
            "syswrapper.sh unset-port-locate"
        ]
    );
}

#[tokio::test]
async fn locate_bad_input_exit_1() {
    let (addr, env) = server_start().await;
    for words in [
        &["locate", "xg10", "1,x"][..],
        &["locate", "xg10", "4-2"][..],
        &["locate", "xg10", "1", "--for", "5y"][..],
        &["locate", "xg10", "13"][..],
        &["locate", "xg10", "1", "--for", "2h"][..],
    ] {
        let output = cli_run(addr, AGENT, None, args(words)).await;
        assert_eq!(
            output.status.code(),
            Some(1),
            "{words:?}: {}",
            output_describe(&output)
        );
    }
    assert!(
        env.fake
            .scripts_get()
            .iter()
            .all(|s| !s.contains("port-locate"))
    );
}

// The reader of the pipe goes away before the client prints, as with `uswctl ports | head`.
#[tokio::test]
async fn closed_stdout_exits_quietly() {
    let (addr, _env) = server_start().await;
    let output = tokio::task::spawn_blocking(move || {
        let mut child = Command::new(env!("CARGO_BIN_EXE_uswctl"))
            .args(["ports", "xg10"])
            .env("USWCTL_URL", format!("http://{addr}"))
            .env("USWCTL_TOKEN", AGENT)
            .env_remove("USWCTL_OWNER")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run the uswctl binary");
        drop(child.stdout.take());
        child
            .wait_with_output()
            .expect("wait for the uswctl binary")
    })
    .await
    .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(stderr.is_empty(), "stderr: {stderr}");
}
