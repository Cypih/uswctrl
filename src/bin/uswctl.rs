// uswctl: the command-line client for the uswctld HTTP API.

use std::io::{ErrorKind as IoErrorKind, Write};

use clap::error::ErrorKind;
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};
use uswctl::backup::stamp_format;
use uswctl::client::{ApiResponse, Client, env_read};
use uswctl::mcp::mcp_serve;
use uswctl::settings::{EdgeMode, PoeMode, PortSettingsPatch};
use uswctl::worker::Action;

// A reader that closed the pipe, as `head` does, wants no more lines: the client stops
// quietly instead of the panic of `println!`.
fn stdout_line_write(args: std::fmt::Arguments) {
    let Err(e) = writeln!(std::io::stdout().lock(), "{args}") else {
        return;
    };
    if e.kind() == IoErrorKind::BrokenPipe {
        std::process::exit(0);
    }
    eprintln!("write to stdout: {e}");
    std::process::exit(1);
}

macro_rules! stdout_line {
    ($($arg:tt)*) => {
        stdout_line_write(format_args!($($arg)*))
    };
}

#[derive(Parser)]
#[command(about = "Command-line client for the uswctld HTTP API")]
struct Cli {
    /// Print the raw JSON response instead of a table.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct PortSetArgs {
    sw: String,
    idx: u8,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    native_vlan: Option<String>,
    #[arg(long)]
    tagged: Option<String>,
    #[arg(long)]
    poe: Option<String>,
    #[arg(long)]
    stp: Option<String>,
    #[arg(long)]
    edge: Option<String>,
    #[arg(long)]
    bpdu_guard: Option<String>,
    #[arg(long)]
    bpdu_filter: Option<String>,
    #[arg(long)]
    enabled: Option<String>,
    #[arg(long)]
    dry_run: bool,
}

#[derive(Subcommand)]
enum Command {
    /// List the configured switches.
    Switches,
    /// Show the ports of one switch.
    Ports { sw: String },
    /// Change settings of one port.
    PortSet(PortSetArgs),
    /// Bring a port's link down or up.
    Link { sw: String, idx: u8, state: String },
    /// Turn a port's PoE off or on, or cycle it.
    Poe { sw: String, idx: u8, action: String },
    /// Reserve a port for USWCTL_OWNER.
    Reserve {
        sw: String,
        idx: u8,
        #[arg(long)]
        ttl: String,
    },
    /// Extend a reservation.
    Extend {
        id: u64,
        #[arg(long)]
        ttl: String,
    },
    /// Release a reservation.
    Release { id: u64 },
    /// List all live reservations.
    Reservations,
    /// Show the MAC table of a switch, or of one of its ports.
    Macs { sw: String, idx: Option<u8> },
    /// Show the recent events of a switch.
    Events { sw: String },
    /// Show the last failure of a switch, if it has one.
    Failure { sw: String },
    /// Make ports blink, to show a person which port to use. A new locate replaces the
    /// running one.
    Locate {
        sw: String,
        /// Ports as a list and ranges, for example 1,2 or 1-4.
        ports: String,
        /// Time until the locate ends (default 5m, from 10s to 1h).
        #[arg(long = "for")]
        duration: Option<String>,
    },
    /// End the locate of a switch at once.
    LocateOff { sw: String },
    /// Serve the API as MCP tools on stdin and stdout, for AI agent sessions.
    Mcp,
}

// The number before an optional trailing unit letter: `90s`, `30m`, `2h`, `1d`, or a plain
// number of seconds.
fn seconds_parse(what: &str, text: &str) -> Result<u64, String> {
    let text = text.trim();
    let bad = || format!("{what} {text:?} is not a number, optionally followed by s, m, h or d");
    let (number, factor) = match text.chars().last() {
        Some('s') => (&text[..text.len() - 1], 1),
        Some('m') => (&text[..text.len() - 1], 60),
        Some('h') => (&text[..text.len() - 1], 3600),
        Some('d') => (&text[..text.len() - 1], 86_400),
        Some(c) if c.is_ascii_alphabetic() => return Err(bad()),
        _ => (text, 1),
    };
    let value: u64 = number.parse().map_err(|_| bad())?;
    value.checked_mul(factor).ok_or_else(bad)
}

fn port_number_parse(text: &str) -> Result<u8, String> {
    text.trim()
        .parse()
        .map_err(|_| format!("port {text:?} is not a port number"))
}

// `1,2` or `1-4`, or a mix such as `1,3-5`. The daemon checks the ports against the switch.
fn ports_list_parse(text: &str) -> Result<Vec<u8>, String> {
    let mut ports = Vec::new();
    for part in text.split(',') {
        match part.split_once('-') {
            Some((first, last)) => {
                let (first, last) = (port_number_parse(first)?, port_number_parse(last)?);
                if first > last {
                    return Err(format!("port range {part:?} ends before it starts"));
                }
                ports.extend(first..=last);
            }
            None => ports.push(port_number_parse(part)?),
        }
    }
    Ok(ports)
}

fn on_off_parse(flag: &str, text: &str) -> Result<bool, String> {
    match text {
        "on" => Ok(true),
        "off" => Ok(false),
        other => Err(format!("--{flag} must be on or off, not {other:?}")),
    }
}

fn poe_parse(text: &str) -> Result<PoeMode, String> {
    match text {
        "auto" => Ok(PoeMode::Auto),
        "off" => Ok(PoeMode::Off),
        other => Err(format!("--poe must be auto or off, not {other:?}")),
    }
}

fn edge_parse(text: &str) -> Result<EdgeMode, String> {
    match text {
        "auto" => Ok(EdgeMode::Auto),
        "on" => Ok(EdgeMode::Enabled),
        "off" => Ok(EdgeMode::Disabled),
        other => Err(format!("--edge must be auto, on or off, not {other:?}")),
    }
}

fn native_vlan_parse(text: &str) -> Result<Option<u16>, String> {
    if text == "none" {
        return Ok(None);
    }
    text.parse()
        .map(Some)
        .map_err(|_| format!("--native-vlan must be a VLAN id or none, not {text:?}"))
}

fn tagged_parse(text: &str) -> Result<Vec<u16>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    text.split(',')
        .map(|part| {
            part.trim()
                .parse()
                .map_err(|_| format!("--tagged has a non-numeric VLAN id: {part:?}"))
        })
        .collect()
}

fn port_patch_build(args: &PortSetArgs) -> Result<PortSettingsPatch, String> {
    let mut patch = PortSettingsPatch {
        name: args.name.clone(),
        ..PortSettingsPatch::default()
    };
    if let Some(text) = &args.native_vlan {
        patch.native_vlan = Some(native_vlan_parse(text)?);
    }
    if let Some(text) = &args.tagged {
        patch.tagged_vlans = Some(tagged_parse(text)?);
    }
    if let Some(text) = &args.poe {
        patch.poe = Some(poe_parse(text)?);
    }
    if let Some(text) = &args.stp {
        patch.stp = Some(on_off_parse("stp", text)?);
    }
    if let Some(text) = &args.edge {
        patch.edge = Some(edge_parse(text)?);
    }
    if let Some(text) = &args.bpdu_guard {
        patch.bpdu_guard = Some(on_off_parse("bpdu-guard", text)?);
    }
    if let Some(text) = &args.bpdu_filter {
        patch.bpdu_filter = Some(on_off_parse("bpdu-filter", text)?);
    }
    if let Some(text) = &args.enabled {
        patch.enabled = Some(on_off_parse("enabled", text)?);
    }
    Ok(patch)
}

fn link_action_parse(state: &str) -> Result<Action, String> {
    match state {
        "down" => Ok(Action::LinkDown),
        "up" => Ok(Action::LinkUp),
        other => Err(format!("link state must be down or up, not {other:?}")),
    }
}

fn poe_action_parse(action: &str) -> Result<Action, String> {
    match action {
        "off" => Ok(Action::PoeOff),
        "on" => Ok(Action::PoeOn),
        "cycle" => Ok(Action::PoeCycle),
        other => Err(format!(
            "poe action must be off, on or cycle, not {other:?}"
        )),
    }
}

// The exit codes of the brief: a refusal (401, 403, 409, 423) is 2, a device failure (422,
// 502, 503) is 3, everything else (400, 404, 500, and a connection error) is 1.
fn exit_code_for_status(status: u16) -> i32 {
    match status {
        401 | 403 | 409 | 423 => 2,
        422 | 502 | 503 => 3,
        _ => 1,
    }
}

fn value_display(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

// `SystemTime` serializes (serde's own impl) as `{"secs_since_epoch": _, "nanos_since_epoch":
// _}`; `stamp_format` turns that into the same readable UTC stamp the backup store uses.
fn time_display(v: &Value) -> String {
    match v.get("secs_since_epoch").and_then(Value::as_u64) {
        Some(secs) => stamp_format(secs),
        None => value_display(v),
    }
}

// A unit `EventKind` serializes as a plain string; a variant with a payload (such as
// `InformConnected`) serializes as `{"VariantName": payload}` (serde's default external
// tagging), shown here as `VariantName: payload`.
fn event_kind_display(v: &Value) -> String {
    match v {
        Value::Object(map) => map.iter().next().map_or_else(String::new, |(k, val)| {
            format!("{k}: {}", value_display(val))
        }),
        other => value_display(other),
    }
}

fn table_print(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let row_print = |cells: &[String]| {
        let line: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, cell)| format!("{cell:<width$}", width = widths[i]))
            .collect();
        stdout_line!("{}", line.join("  ").trim_end());
    };
    let header_cells: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    row_print(&header_cells);
    for row in rows {
        row_print(row);
    }
}

fn ports_display(v: &Value) -> String {
    let ports: Vec<String> = v
        .as_array()
        .into_iter()
        .flatten()
        .map(value_display)
        .collect();
    ports.join(",")
}

fn locate_display(locate: &Value) -> String {
    if locate.is_null() {
        return String::new();
    }
    format!(
        "{} until {}",
        ports_display(&locate["ports"]),
        time_display(&locate["until"])
    )
}

fn switches_print(body: &Value) {
    let rows: Vec<Vec<String>> = body
        .as_array()
        .into_iter()
        .flatten()
        .map(|sw| {
            vec![
                value_display(&sw["name"]),
                value_display(&sw["model"]),
                value_display(&sw["firmware"]),
                value_display(&sw["firmware_tested"]),
                value_display(&sw["uptime_s"]),
                locate_display(&sw["locate"]),
            ]
        })
        .collect();
    table_print(
        &["NAME", "MODEL", "FIRMWARE", "TESTED", "UPTIME_S", "LOCATE"],
        &rows,
    );
}

fn vlans_display(v: &Value) -> String {
    let ids: Vec<String> = v
        .as_array()
        .into_iter()
        .flatten()
        .map(value_display)
        .collect();
    if ids.is_empty() {
        "none".to_string()
    } else {
        ids.join(",")
    }
}

fn native_display(v: &Value) -> String {
    if v.is_null() {
        "none".to_string()
    } else {
        value_display(v)
    }
}

// A port without VLAN membership lines carries every VLAN of the switch, so its tagged
// list is not a list of its own.
fn tagged_display(settings: &Value) -> String {
    if settings["vlan_all"].as_bool() == Some(true) {
        "all".to_string()
    } else {
        vlans_display(&settings["tagged_vlans"])
    }
}

fn global_line_build(global: &Value) -> String {
    format!(
        "STP version {}, STP priority {}, BPDU action {}, VLANs {}",
        value_display(&global["stp_version"]),
        value_display(&global["stp_priority"]),
        value_display(&global["bpdu_action"]),
        vlans_display(&global["vlans"]),
    )
}

fn ports_print(body: &Value) {
    stdout_line!("{}", global_line_build(&body["global"]));
    let locate = &body["locate"];
    if !locate.is_null() {
        let retry = if locate["end_failed"].as_bool() == Some(true) {
            "; ending the locate failed; retrying"
        } else {
            ""
        };
        stdout_line!("Locate: ports {}{retry}", locate_display(locate));
    }
    let rows: Vec<Vec<String>> = body["ports"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| {
            let settings = &p["settings"];
            vec![
                value_display(&p["idx"]),
                value_display(&settings["name"]),
                value_display(&p["live"]["up"]),
                value_display(&p["live"]["speed_mbps"]),
                native_display(&settings["native_vlan"]),
                tagged_display(settings),
                value_display(&settings["poe"]),
                value_display(&settings["stp"]),
                value_display(&settings["bpdu_filter"]),
                value_display(&settings["enabled"]),
                value_display(&p["protected"]),
            ]
        })
        .collect();
    table_print(
        &[
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
            "PROTECTED",
        ],
        &rows,
    );
}

fn change_result_print(body: &Value) {
    let rows: Vec<Vec<String>> = body["keys"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|k| {
            vec![
                value_display(&k["key"]),
                value_display(&k["old"]),
                value_display(&k["new"]),
            ]
        })
        .collect();
    table_print(&["KEY", "OLD", "NEW"], &rows);
    stdout_line!(
        "overlay changed: {}",
        value_display(&body["overlay_changed"])
    );
    stdout_line!("readback ok: {}", value_display(&body["readback_ok"]));
}

fn action_result_print(body: &Value) {
    stdout_line!("switch: {}", value_display(&body["switch"]));
    stdout_line!("port: {}", value_display(&body["port"]));
    stdout_line!("action: {}", value_display(&body["action"]));
    cli_lines_print(body);
}

fn cli_lines_print(body: &Value) {
    for line in body["cli_lines"].as_array().into_iter().flatten() {
        stdout_line!("  {}", value_display(line));
    }
}

fn locate_result_print(body: &Value) {
    stdout_line!("switch: {}", value_display(&body["switch"]));
    stdout_line!("ports: {}", ports_display(&body["ports"]));
    stdout_line!("until: {}", time_display(&body["until"]));
    cli_lines_print(body);
}

fn locate_off_print(body: &Value) {
    stdout_line!("switch: {}", value_display(&body["switch"]));
    stdout_line!("locate ended");
    cli_lines_print(body);
}

fn reservation_print(body: &Value) {
    table_print(
        &["ID", "SWITCH", "PORT", "OWNER", "CREATED", "EXPIRES"],
        &[vec![
            value_display(&body["id"]),
            value_display(&body["switch"]),
            value_display(&body["port"]),
            value_display(&body["owner"]),
            time_display(&body["created"]),
            time_display(&body["expires"]),
        ]],
    );
}

fn reservations_print(body: &Value) {
    let rows: Vec<Vec<String>> = body
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            vec![
                value_display(&r["id"]),
                value_display(&r["switch"]),
                value_display(&r["port"]),
                value_display(&r["owner"]),
                time_display(&r["created"]),
                time_display(&r["expires"]),
            ]
        })
        .collect();
    table_print(
        &["ID", "SWITCH", "PORT", "OWNER", "CREATED", "EXPIRES"],
        &rows,
    );
}

fn macs_print(body: &Value, idx: Option<u8>) {
    let mut rows: Vec<Vec<String>> = Vec::new();
    for port in body["ports"].as_array().into_iter().flatten() {
        let port_idx = port["idx"].as_u64().unwrap_or_default();
        if idx.is_some_and(|wanted| port_idx != u64::from(wanted)) {
            continue;
        }
        for mac in port["live"]["macs"].as_array().into_iter().flatten() {
            rows.push(vec![port_idx.to_string(), value_display(mac)]);
        }
    }
    table_print(&["PORT", "MAC"], &rows);
}

fn events_print(body: &Value) {
    let rows: Vec<Vec<String>> = body["events"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            vec![
                event_kind_display(&e["kind"]),
                time_display(&e["at"]),
                value_display(&e["requested"]),
            ]
        })
        .collect();
    table_print(&["KIND", "AT", "REQUESTED"], &rows);
}

fn failure_print(body: &Value) {
    if let Some(text) = body["last_failure"].as_str() {
        stdout_line!("{text}");
    }
}

fn released_print(_body: &Value) {
    stdout_line!("released");
}

fn outcome_handle(
    result: Result<ApiResponse, ureq::Error>,
    json_flag: bool,
    table_print: impl FnOnce(&Value),
) -> i32 {
    let response = match result {
        Ok(response) => response,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    if response.status / 100 != 2 {
        eprintln!("{}", response.body);
        return exit_code_for_status(response.status);
    }
    if json_flag {
        match serde_json::to_string_pretty(&response.body) {
            Ok(text) => stdout_line!("{text}"),
            Err(_) => stdout_line!("{}", response.body),
        }
    } else {
        table_print(&response.body);
    }
    0
}

fn action_run(client: &Client, json_flag: bool, sw: &str, idx: u8, action: Action) -> i32 {
    let body = json!({ "action": action });
    outcome_handle(
        client.post(&format!("/api/switches/{sw}/ports/{idx}/actions"), &body),
        json_flag,
        action_result_print,
    )
}

fn command_run(cli: &Cli, client: &Client) -> i32 {
    match &cli.command {
        Command::Switches => outcome_handle(client.get("/api/switches"), cli.json, switches_print),
        Command::Ports { sw } => outcome_handle(
            client.get(&format!("/api/switches/{sw}")),
            cli.json,
            ports_print,
        ),
        Command::PortSet(args) => {
            let patch = match port_patch_build(args) {
                Ok(patch) => patch,
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            };
            let body = serde_json::to_value(&patch).expect("PortSettingsPatch always serializes");
            let path = if args.dry_run {
                format!("/api/switches/{}/ports/{}?dry_run=1", args.sw, args.idx)
            } else {
                format!("/api/switches/{}/ports/{}", args.sw, args.idx)
            };
            outcome_handle(client.patch(&path, &body), cli.json, change_result_print)
        }
        Command::Link { sw, idx, state } => {
            let action = match link_action_parse(state) {
                Ok(action) => action,
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            };
            action_run(client, cli.json, sw, *idx, action)
        }
        Command::Poe { sw, idx, action } => {
            let action = match poe_action_parse(action) {
                Ok(action) => action,
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            };
            action_run(client, cli.json, sw, *idx, action)
        }
        Command::Reserve { sw, idx, ttl } => {
            let ttl_s = match seconds_parse("ttl", ttl) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            };
            let Some(owner) = client.owner_get() else {
                eprintln!("USWCTL_OWNER is not set");
                return 1;
            };
            let body = json!({ "switch": sw, "port": idx, "owner": owner, "ttl_s": ttl_s });
            outcome_handle(
                client.post("/api/reservations", &body),
                cli.json,
                reservation_print,
            )
        }
        Command::Extend { id, ttl } => {
            let ttl_s = match seconds_parse("ttl", ttl) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            };
            let body = json!({ "ttl_s": ttl_s });
            outcome_handle(
                client.post(&format!("/api/reservations/{id}/extend"), &body),
                cli.json,
                reservation_print,
            )
        }
        Command::Release { id } => outcome_handle(
            client.delete(&format!("/api/reservations/{id}")),
            cli.json,
            released_print,
        ),
        Command::Reservations => outcome_handle(
            client.get("/api/reservations"),
            cli.json,
            reservations_print,
        ),
        Command::Macs { sw, idx } => {
            let idx = *idx;
            outcome_handle(
                client.get(&format!("/api/switches/{sw}")),
                cli.json,
                |body| macs_print(body, idx),
            )
        }
        Command::Events { sw } => outcome_handle(
            client.get(&format!("/api/switches/{sw}")),
            cli.json,
            events_print,
        ),
        Command::Failure { sw } => outcome_handle(
            client.get(&format!("/api/switches/{sw}")),
            cli.json,
            failure_print,
        ),
        Command::Locate {
            sw,
            ports,
            duration,
        } => {
            let request = || -> Result<Value, String> {
                let ports = ports_list_parse(ports)?;
                let mut body = json!({ "ports": ports });
                if let Some(text) = duration {
                    body["duration_s"] = json!(seconds_parse("duration", text)?);
                }
                Ok(body)
            };
            let body = match request() {
                Ok(body) => body,
                Err(e) => {
                    eprintln!("{e}");
                    return 1;
                }
            };
            outcome_handle(
                client.post(&format!("/api/switches/{sw}/locate"), &body),
                cli.json,
                locate_result_print,
            )
        }
        Command::LocateOff { sw } => outcome_handle(
            client.delete(&format!("/api/switches/{sw}/locate")),
            cli.json,
            locate_off_print,
        ),
        Command::Mcp => mcp_run(client),
    }
}

// stdout carries the MCP protocol, so every log line goes to stderr.
fn mcp_run(client: &Client) -> i32 {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("start the async runtime: {e}");
            return 1;
        }
    };
    match runtime.block_on(mcp_serve(client.clone())) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("mcp: {e:#}");
            1
        }
    }
}

// clap's own `.exit()` uses exit code 2 for every parse error, which the brief's exit codes
// already give to a server-side refusal (401, 403, 409, 423); a script could then not tell
// "bad command line" from "refused by the server". So a parse error here always exits 0
// (`--help` or `--version`, which the caller asked for) or 1 (every other parse error,
// including a missing subcommand: that one also prints help, but the caller did not ask for
// it), never 2.
fn cli_parse_or_exit() -> Cli {
    match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            let code = match e.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => 0,
                _ => 1,
            };
            std::process::exit(code);
        }
    }
}

fn main() {
    let cli = cli_parse_or_exit();
    let env = match env_read() {
        Ok(env) => env,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    let client = Client::client_build(&env);
    std::process::exit(command_run(&cli, &client));
}
