// The MCP server of `uswctl mcp`: typed tools over stdio that call the uswctld HTTP API.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    InitializeResult, JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
    ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{Value, json};

use crate::client::{ApiResponse, Client};
use crate::locate::{LOCATE_DURATION_DEFAULT, LOCATE_DURATION_MAX_S, LOCATE_DURATION_MIN_S};
use crate::settings::PortSettingsPatch;
use crate::worker::Action;

const INSTRUCTIONS: &str = "Tools for the UniFi switches behind uswctld. \
Each agent session must pass its own session name as owner on every write tool and \
reservation tool (port_set, port_action, port_reserve, reservation_extend, \
reservation_release); other sessions can share this server. \
port_set is a dry run unless you pass dry_run: false. \
A real port_set and every port_action need a reservation of that port by your owner name: \
call port_reserve first, and reservation_release when you are done. \
port_locate makes ports blink, to show a person which port to use; it needs no reservation \
and it ends by itself. Pass owner to it as well, so that the daemon log shows your session.";

const SWITCH_PATTERN: &str = "^[A-Za-z0-9._-]+$";

// A tool with no argument still takes an object, and refuses every key.
fn schema_empty_build() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

fn schema_build(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn switch_property_build() -> Value {
    json!({
        "type": "string",
        "pattern": SWITCH_PATTERN,
        "description": "Switch name, as switches_list shows it.",
    })
}

fn port_property_build() -> Value {
    json!({ "type": "integer", "minimum": 1, "description": "Port number, from 1." })
}

fn owner_property_build() -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "description": "Your agent session name. Each session must pass its own name on every \
            write tool and reservation tool, so that reservations keep sessions apart. \
            Default: USWCTL_OWNER of the server.",
    })
}

fn ttl_property_build() -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "maximum": 86400,
        "description": "Time to live of the reservation in seconds, counted from now.",
    })
}

fn id_property_build() -> Value {
    json!({ "type": "integer", "minimum": 1, "description": "Reservation id." })
}

fn port_set_schema_build() -> Value {
    schema_build(
        json!({
            "switch": switch_property_build(),
            "port": port_property_build(),
            "name": { "type": "string", "description": "Port name." },
            "native_vlan": {
                "type": ["integer", "null"],
                "minimum": 1,
                "maximum": 4094,
                "description": "Native (untagged) VLAN id, or null for no native VLAN.",
            },
            "tagged_vlans": {
                "type": "array",
                "items": { "type": "integer", "minimum": 1, "maximum": 4094 },
                "description": "The full list of tagged VLAN ids. It replaces the current list.",
            },
            "poe": { "type": "string", "enum": ["auto", "off"], "description": "PoE mode." },
            "stp": { "type": "boolean", "description": "Spanning tree on the port." },
            "edge": {
                "type": "string",
                "enum": ["auto", "enabled", "disabled"],
                "description": "STP edge port mode.",
            },
            "bpdu_guard": { "type": "boolean", "description": "BPDU guard." },
            "bpdu_filter": { "type": "boolean", "description": "BPDU filter." },
            "enabled": { "type": "boolean", "description": "Port enabled." },
            "dry_run": {
                "type": "boolean",
                "default": true,
                "description": "If true (the default), the daemon checks the change and shows it, \
                    and does not write it. Pass false to write.",
            },
            "owner": owner_property_build(),
        }),
        &["switch", "port"],
    )
}

fn port_action_schema_build() -> Value {
    schema_build(
        json!({
            "switch": switch_property_build(),
            "port": port_property_build(),
            "action": {
                "type": "string",
                "enum": ["link_down", "link_up", "poe_off", "poe_on", "poe_cycle"],
                "description": "Transient action. The switch does not keep it after a reboot.",
            },
            "owner": owner_property_build(),
        }),
        &["switch", "port", "action"],
    )
}

fn port_locate_schema_build() -> Value {
    schema_build(
        json!({
            "switch": switch_property_build(),
            "ports": {
                "type": "array",
                "items": { "type": "integer", "minimum": 1 },
                "minItems": 1,
                "uniqueItems": true,
                "description": "The ports that blink. A new locate replaces the running one.",
            },
            "duration_s": {
                "type": "integer",
                "minimum": LOCATE_DURATION_MIN_S,
                "maximum": LOCATE_DURATION_MAX_S,
                "default": LOCATE_DURATION_DEFAULT.as_secs(),
                "description": "Seconds until the locate ends by itself.",
            },
            "owner": owner_property_build(),
        }),
        &["switch", "ports"],
    )
}

type ToolRun = fn(&Client, &JsonObject) -> Result<Value, String>;

struct ToolSpec {
    name: &'static str,
    description: &'static str,
    schema: fn() -> Value,
    run: ToolRun,
    read_only: bool,
    destructive: bool,
}

const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "switches_list",
        description: "List the switches: name, model, firmware, firmware tested, uptime.",
        schema: schema_empty_build,
        run: switches_list_run,
        read_only: true,
        destructive: false,
    },
    ToolSpec {
        name: "ports_list",
        description: "List the ports of one switch as compact rows: link, speed, PoE watts, \
            VLANs, STP, BPDU filter, enabled, protected and the reservation owner.",
        schema: || schema_build(json!({ "switch": switch_property_build() }), &["switch"]),
        run: ports_list_run,
        read_only: true,
        destructive: false,
    },
    ToolSpec {
        name: "port_get",
        description: "Get the full state of one port: live data and settings.",
        schema: || {
            schema_build(
                json!({ "switch": switch_property_build(), "port": port_property_build() }),
                &["switch", "port"],
            )
        },
        run: port_get_run,
        read_only: true,
        destructive: false,
    },
    ToolSpec {
        name: "macs_get",
        description: "Get the MAC addresses that a switch learned, for each port or for one port.",
        schema: || {
            schema_build(
                json!({ "switch": switch_property_build(), "port": port_property_build() }),
                &["switch"],
            )
        },
        run: macs_get_run,
        read_only: true,
        destructive: false,
    },
    ToolSpec {
        name: "events_get",
        description: "Get the recent events of a switch and its last failure, if it has one.",
        schema: || schema_build(json!({ "switch": switch_property_build() }), &["switch"]),
        run: events_get_run,
        read_only: true,
        destructive: false,
    },
    ToolSpec {
        name: "reservations_list",
        description: "List all live port reservations.",
        schema: schema_empty_build,
        run: reservations_list_run,
        read_only: true,
        destructive: false,
    },
    ToolSpec {
        name: "port_set",
        description: "Change settings of one port. A dry run by default: pass dry_run false to \
            write. A write needs a reservation of the port by the owner.",
        schema: port_set_schema_build,
        run: port_set_run,
        read_only: false,
        destructive: true,
    },
    ToolSpec {
        name: "port_action",
        description: "Bring a port's link down or up, or turn its PoE off, on, or cycle it. \
            Needs a reservation of the port by the owner.",
        schema: port_action_schema_build,
        run: port_action_run,
        read_only: false,
        destructive: true,
    },
    ToolSpec {
        name: "port_locate",
        description: "Make ports of a switch blink blue, to show a person which port to use. \
            All other port LEDs go dark until the locate ends. Needs no reservation, also on \
            a protected port.",
        schema: port_locate_schema_build,
        run: port_locate_run,
        read_only: false,
        destructive: false,
    },
    ToolSpec {
        name: "port_locate_off",
        description: "End the locate of a switch at once and give the port LEDs back.",
        schema: || {
            schema_build(
                json!({ "switch": switch_property_build(), "owner": owner_property_build() }),
                &["switch"],
            )
        },
        run: port_locate_off_run,
        read_only: false,
        destructive: false,
    },
    ToolSpec {
        name: "port_reserve",
        description: "Reserve a port for the owner for ttl_s seconds.",
        schema: || {
            schema_build(
                json!({
                    "switch": switch_property_build(),
                    "port": port_property_build(),
                    "ttl_s": ttl_property_build(),
                    "owner": owner_property_build(),
                }),
                &["switch", "port", "ttl_s"],
            )
        },
        run: port_reserve_run,
        read_only: false,
        destructive: false,
    },
    ToolSpec {
        name: "reservation_extend",
        description: "Set a new expiry on a reservation of the owner, ttl_s seconds from now.",
        schema: || {
            schema_build(
                json!({
                    "id": id_property_build(),
                    "ttl_s": ttl_property_build(),
                    "owner": owner_property_build(),
                }),
                &["id", "ttl_s"],
            )
        },
        run: reservation_extend_run,
        read_only: false,
        destructive: false,
    },
    ToolSpec {
        name: "reservation_release",
        description: "Release a reservation of the owner.",
        schema: || {
            schema_build(
                json!({ "id": id_property_build(), "owner": owner_property_build() }),
                &["id"],
            )
        },
        run: reservation_release_run,
        read_only: false,
        destructive: false,
    },
];

fn tool_spec_find(name: &str) -> Option<&'static ToolSpec> {
    TOOLS.iter().find(|spec| spec.name == name)
}

fn tool_build(spec: &ToolSpec) -> Tool {
    let schema = match (spec.schema)() {
        Value::Object(map) => map,
        _ => unreachable!("every tool schema is a JSON object"),
    };
    let annotations = ToolAnnotations::new()
        .read_only(spec.read_only)
        .destructive(spec.destructive);
    Tool::new(spec.name, spec.description, schema).with_annotations(annotations)
}

// The client may skip the schema, so the server checks the keys itself: an unknown key is
// usually a typo, and a typo must not turn into a change with a field left out.
fn arguments_check(spec: &ToolSpec, args: &JsonObject) -> Result<(), String> {
    let schema = (spec.schema)();
    let properties = schema["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    if let Some(key) = args.keys().find(|key| !properties.contains_key(*key)) {
        return Err(format!(
            "argument {key:?} is not an argument of {}",
            spec.name
        ));
    }
    let required = schema["required"].as_array().cloned().unwrap_or_default();
    if let Some(key) = required
        .iter()
        .filter_map(Value::as_str)
        .find(|key| !args.contains_key(*key))
    {
        return Err(format!("argument {key:?} is required by {}", spec.name));
    }
    Ok(())
}

fn string_arg_get(args: &JsonObject, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(other) => Err(format!("argument {key:?} must be a string, not {other}")),
    }
}

fn integer_arg_get(args: &JsonObject, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("argument {key:?} must be a whole number, not {value}")),
    }
}

fn integer_list_arg_require(args: &JsonObject, key: &str) -> Result<Vec<u64>, String> {
    let bad =
        |value: &Value| format!("argument {key:?} must be a list of whole numbers, not {value}");
    match args.get(key) {
        None | Some(Value::Null) => Err(format!("argument {key:?} is required")),
        Some(value @ Value::Array(items)) => items
            .iter()
            .map(|item| item.as_u64().ok_or_else(|| bad(value)))
            .collect(),
        Some(value) => Err(bad(value)),
    }
}

fn integer_arg_require(args: &JsonObject, key: &str) -> Result<u64, String> {
    integer_arg_get(args, key)?.ok_or_else(|| format!("argument {key:?} is required"))
}

// The switch name goes into the URL path, so a "/", "?" or "#" in it, or a name of dots
// only ("." or ".."), could aim the request at another port or route than the one the
// reservation check looked at.
fn switch_arg_get(args: &JsonObject) -> Result<String, String> {
    let name = string_arg_get(args, "switch")?.ok_or("argument \"switch\" is required")?;
    let valid = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !name.chars().all(|c| c == '.');
    if !valid {
        return Err(format!(
            "switch name {name:?} must match {SWITCH_PATTERN} and must not be dots only"
        ));
    }
    Ok(name)
}

fn owner_arg_get(args: &JsonObject) -> Result<Option<String>, String> {
    Ok(string_arg_get(args, "owner")?
        .map(|o| o.trim().to_string())
        .filter(|o| !o.is_empty()))
}

fn owner_resolve(client: &Client, args: &JsonObject) -> Result<String, String> {
    owner_arg_get(args)?
        .or_else(|| client.owner_get().map(str::to_string))
        .ok_or_else(|| {
            "this tool needs an owner name: pass the owner argument or set USWCTL_OWNER \
         in the environment of the MCP server"
                .to_string()
        })
}

fn api_error_describe(response: &ApiResponse) -> String {
    let body = &response.body;
    match (body["error"].as_str(), body["message"].as_str()) {
        (Some(kind), Some(message)) => format!("HTTP {} {kind}: {message}", response.status),
        _ => format!("HTTP {}: {body}", response.status),
    }
}

fn api_result_get(result: Result<ApiResponse, ureq::Error>) -> Result<Value, String> {
    let response = result.map_err(|e| format!("request to uswctld failed: {e}"))?;
    if response.status / 100 != 2 {
        return Err(api_error_describe(&response));
    }
    Ok(response.body)
}

fn switch_state_get(client: &Client, switch: &str) -> Result<Value, String> {
    api_result_get(client.get(&format!("/api/switches/{switch}")))
}

fn reservations_get(client: &Client) -> Result<Vec<Value>, String> {
    let body = api_result_get(client.get("/api/reservations"))?;
    match body {
        Value::Array(list) => Ok(list),
        other => Err(format!(
            "uswctld sent a reservation list that is not a list: {other}"
        )),
    }
}

fn reservation_find<'a>(reservations: &'a [Value], switch: &str, port: u64) -> Option<&'a Value> {
    reservations
        .iter()
        .find(|r| r["switch"] == switch && r["port"].as_u64() == Some(port))
}

// Stricter than the daemon, which lets any agent change a port that has no reservation:
// agents share the agent token, so only the reservation tells them apart.
fn reservation_held_check(
    client: &Client,
    switch: &str,
    port: u64,
    owner: &str,
) -> Result<(), String> {
    let reservations = reservations_get(client)?;
    let Some(reservation) = reservation_find(&reservations, switch, port) else {
        return Err(format!(
            "port {port} of switch {switch} has no reservation; reserve it for {owner} \
             with port_reserve first"
        ));
    };
    let holder = reservation["owner"].as_str().unwrap_or_default();
    if holder != owner {
        return Err(format!(
            "the reservation of port {port} of switch {switch} belongs to {holder}, not to {owner}"
        ));
    }
    Ok(())
}

fn port_view_find(state: &Value, switch: &str, port: u64) -> Result<Value, String> {
    state["ports"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["idx"].as_u64() == Some(port))
        .cloned()
        .ok_or_else(|| format!("switch {switch} has no port {port}"))
}

fn port_row_build(port: &Value, reservations: &[Value], switch: &str) -> Value {
    let settings = &port["settings"];
    let live = &port["live"];
    let reserved_by = port["idx"]
        .as_u64()
        .and_then(|idx| reservation_find(reservations, switch, idx))
        .map_or(Value::Null, |r| r["owner"].clone());
    json!({
        "idx": port["idx"],
        "name": settings["name"],
        "cli_name": port["cli_name"],
        "up": live["up"],
        "speed_mbps": live["speed_mbps"],
        "poe_w": live["poe_power_w"],
        "native_vlan": settings["native_vlan"],
        "tagged_vlans": settings["tagged_vlans"],
        "vlan_all": settings["vlan_all"],
        "stp": settings["stp"],
        "bpdu_filter": settings["bpdu_filter"],
        "enabled": settings["enabled"],
        "protected": port["protected"],
        "reserved_by": reserved_by,
    })
}

fn switches_list_run(client: &Client, _args: &JsonObject) -> Result<Value, String> {
    api_result_get(client.get("/api/switches"))
}

fn reservations_list_run(client: &Client, _args: &JsonObject) -> Result<Value, String> {
    api_result_get(client.get("/api/reservations"))
}

fn ports_list_run(client: &Client, args: &JsonObject) -> Result<Value, String> {
    let switch = switch_arg_get(args)?;
    let state = switch_state_get(client, &switch)?;
    let reservations = reservations_get(client)?;
    let rows: Vec<Value> = state["ports"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| port_row_build(p, &reservations, &switch))
        .collect();
    Ok(json!({ "switch": switch, "global": state["global"], "ports": rows }))
}

fn port_get_run(client: &Client, args: &JsonObject) -> Result<Value, String> {
    let switch = switch_arg_get(args)?;
    let port = integer_arg_require(args, "port")?;
    let state = switch_state_get(client, &switch)?;
    port_view_find(&state, &switch, port)
}

fn macs_get_run(client: &Client, args: &JsonObject) -> Result<Value, String> {
    let switch = switch_arg_get(args)?;
    let port = integer_arg_get(args, "port")?;
    let state = switch_state_get(client, &switch)?;
    let views = match port {
        Some(port) => vec![port_view_find(&state, &switch, port)?],
        None => state["ports"].as_array().cloned().unwrap_or_default(),
    };
    let rows: Vec<Value> = views
        .iter()
        .map(|p| json!({ "port": p["idx"], "macs": p["live"]["macs"] }))
        .collect();
    Ok(Value::Array(rows))
}

fn events_get_run(client: &Client, args: &JsonObject) -> Result<Value, String> {
    let switch = switch_arg_get(args)?;
    let state = switch_state_get(client, &switch)?;
    Ok(json!({ "events": state["events"], "last_failure": state["last_failure"] }))
}

// Every key that is not a tool argument is a patch field, so that `PortSettingsPatch` and
// its serde rules (enums, null for no native VLAN, unknown fields) stay the one definition.
fn port_patch_parse(args: &JsonObject) -> Result<Value, String> {
    let fields: JsonObject = args
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "switch" | "port" | "dry_run" | "owner"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    // A serde error for an enum value does not name its field; one field at a time does.
    for (key, value) in &fields {
        serde_json::from_value::<PortSettingsPatch>(json!({ key: value }))
            .map_err(|e| format!("argument {key:?}: {e}"))?;
    }
    let patch: PortSettingsPatch =
        serde_json::from_value(Value::Object(fields)).map_err(|e| format!("port settings: {e}"))?;
    serde_json::to_value(&patch).map_err(|e| format!("port settings: {e}"))
}

fn port_set_run(client: &Client, args: &JsonObject) -> Result<Value, String> {
    let switch = switch_arg_get(args)?;
    let port = integer_arg_require(args, "port")?;
    let dry_run = match args.get("dry_run") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(flag)) => *flag,
        Some(other) => {
            return Err(format!(
                "argument \"dry_run\" must be true or false, not {other}"
            ));
        }
    };
    let owner = owner_resolve(client, args)?;
    let body = port_patch_parse(args)?;
    let client = client.client_owner_set(&owner);
    let path = if dry_run {
        format!("/api/switches/{switch}/ports/{port}?dry_run=1")
    } else {
        reservation_held_check(&client, &switch, port, &owner)?;
        format!("/api/switches/{switch}/ports/{port}")
    };
    api_result_get(client.patch(&path, &body))
}

fn port_action_run(client: &Client, args: &JsonObject) -> Result<Value, String> {
    let switch = switch_arg_get(args)?;
    let port = integer_arg_require(args, "port")?;
    let action: Action = serde_json::from_value(args.get("action").cloned().unwrap_or_default())
        .map_err(|e| format!("argument \"action\": {e}"))?;
    let owner = owner_resolve(client, args)?;
    let client = client.client_owner_set(&owner);
    reservation_held_check(&client, &switch, port, &owner)?;
    let body = json!({ "action": action });
    api_result_get(client.post(
        &format!("/api/switches/{switch}/ports/{port}/actions"),
        &body,
    ))
}

// A locate needs no owner. An owner that the call names goes to the daemon in the owner
// header, so that the daemon log shows the session.
fn client_owner_apply(client: &Client, args: &JsonObject) -> Result<Client, String> {
    Ok(match owner_arg_get(args)? {
        Some(owner) => client.client_owner_set(&owner),
        None => client.clone(),
    })
}

fn port_locate_run(client: &Client, args: &JsonObject) -> Result<Value, String> {
    let switch = switch_arg_get(args)?;
    let client = client_owner_apply(client, args)?;
    let ports = integer_list_arg_require(args, "ports")?;
    let mut body = json!({ "ports": ports });
    if let Some(duration_s) = integer_arg_get(args, "duration_s")? {
        body["duration_s"] = json!(duration_s);
    }
    api_result_get(client.post(&format!("/api/switches/{switch}/locate"), &body))
}

fn port_locate_off_run(client: &Client, args: &JsonObject) -> Result<Value, String> {
    let switch = switch_arg_get(args)?;
    let client = client_owner_apply(client, args)?;
    api_result_get(client.delete(&format!("/api/switches/{switch}/locate")))
}

fn port_reserve_run(client: &Client, args: &JsonObject) -> Result<Value, String> {
    let switch = switch_arg_get(args)?;
    let port = integer_arg_require(args, "port")?;
    let ttl_s = integer_arg_require(args, "ttl_s")?;
    let owner = owner_resolve(client, args)?;
    let client = client.client_owner_set(&owner);
    let body = json!({ "switch": switch, "port": port, "owner": owner, "ttl_s": ttl_s });
    api_result_get(client.post("/api/reservations", &body))
}

fn reservation_extend_run(client: &Client, args: &JsonObject) -> Result<Value, String> {
    let id = integer_arg_require(args, "id")?;
    let ttl_s = integer_arg_require(args, "ttl_s")?;
    let owner = owner_resolve(client, args)?;
    let client = client.client_owner_set(&owner);
    let body = json!({ "ttl_s": ttl_s });
    api_result_get(client.post(&format!("/api/reservations/{id}/extend"), &body))
}

fn reservation_release_run(client: &Client, args: &JsonObject) -> Result<Value, String> {
    let id = integer_arg_require(args, "id")?;
    let owner = owner_resolve(client, args)?;
    let client = client.client_owner_set(&owner);
    api_result_get(client.delete(&format!("/api/reservations/{id}")))
}

fn tool_run(client: &Client, spec: &ToolSpec, args: &JsonObject) -> Result<Value, String> {
    arguments_check(spec, args)?;
    (spec.run)(client, args)
}

struct McpServer {
    client: Arc<Client>,
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("uswctl", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(
            TOOLS.iter().map(tool_build).collect(),
        ))
    }

    // A failure of the tool itself is a tool result with isError, so that the agent reads
    // the reason; only an unknown tool is a JSON-RPC error, as the MCP specification says.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some(spec) = tool_spec_find(&request.name) else {
            return Err(ErrorData::invalid_params(
                format!("unknown tool: {}", request.name),
                None,
            ));
        };
        let client = self.client.clone();
        let args = request.arguments.unwrap_or_default();
        // The client is blocking (ureq); a write can take minutes on a real switch.
        let outcome = tokio::task::spawn_blocking(move || tool_run(&client, spec, &args))
            .await
            .map_err(|e| ErrorData::internal_error(format!("tool {}: {e}", spec.name), None))?;
        let result = match outcome {
            Ok(body) => {
                tracing::info!("tool {}: ok", spec.name);
                CallToolResult::success(vec![ContentBlock::text(body.to_string())])
            }
            Err(text) => {
                tracing::warn!("tool {}: {text}", spec.name);
                CallToolResult::error(vec![ContentBlock::text(text)])
            }
        };
        Ok(result.into())
    }
}

/// Serves MCP on stdin and stdout until the client closes stdin. The caller must send every
/// log line to stderr, because stdout carries the protocol.
pub async fn mcp_serve(client: Client) -> anyhow::Result<()> {
    let server = McpServer {
        client: Arc::new(client),
    };
    let running = server.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}
