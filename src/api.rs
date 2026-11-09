// The HTTP API of the daemon (spec section 9): roles, safety rules, reservations and the web files.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Extension, Path, RawQuery, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::{Refusal, Role, Target, role_from_headers, safety_check};
use crate::backup::{BackupStore, stamp_format};
use crate::change::{Change, ChangeError, ChangeResult, SwitchCtx};
use crate::config::{Config, SwitchConfig};
use crate::locate::{
    LOCATE_DURATION_DEFAULT, LOCATE_DURATION_MAX_S, LOCATE_DURATION_MIN_S, locate_ports_check,
};
use crate::model::{model_from_code, model_port_count};
use crate::reserve::{Reservation, ReservationStore};
use crate::settings::{GlobalSettingsPatch, PortSettingsPatch};
use crate::transport::Transport;
use crate::worker::{Action, DeviceHandle, DeviceState, Event, Locate, worker_spawn};

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");

const TTL_MAX_S: u64 = 86_400;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub devices: BTreeMap<String, DeviceHandle>,
    pub reservations: Arc<ReservationStore>,
    pub backups: BackupStore,
}

type Shared = Arc<AppState>;

/// Starts one device worker for each switch of `cfg`, over the transport that
/// `transport_for` gives for that switch, and opens the stores in `cfg.data_dir`. The
/// daemon passes SSH transports; the tests and `uswctld --fake` pass simulated ones.
pub fn app_state_build(
    cfg: Config,
    transport_for: impl Fn(&SwitchConfig) -> anyhow::Result<Arc<dyn Transport>>,
) -> anyhow::Result<AppState> {
    let backups = BackupStore {
        dir: cfg.data_dir.join("backups"),
    };
    let mut devices = BTreeMap::new();
    for sw in &cfg.switches {
        let model = model_from_code(&sw.model_code).ok_or_else(|| {
            anyhow::anyhow!("switch {}: unknown model {}", sw.name, sw.model_code)
        })?;
        let ctx = SwitchCtx {
            name: sw.name.clone(),
            model,
            lock_path: cfg.lock_path.clone(),
            lock_token: sw.lock_token.clone(),
            tested_firmware: sw.tested_firmware.clone(),
        };
        let handle = worker_spawn(
            transport_for(sw)?,
            ctx,
            sw.protected_ports.clone(),
            backups.clone(),
            Duration::from_secs(cfg.poll_s),
            Duration::from_secs(cfg.overlay_check_s),
        )?;
        devices.insert(sw.name.clone(), handle);
    }
    let reservations =
        ReservationStore::reservation_store_open(cfg.data_dir.join("reservations.json"));
    Ok(AppState {
        cfg: Arc::new(cfg),
        devices,
        reservations: Arc::new(reservations),
        backups,
    })
}

#[derive(Debug, Clone)]
pub struct ApiError {
    status: StatusCode,
    kind: &'static str,
    message: String,
}

fn api_error(status: StatusCode, kind: &'static str, message: impl Into<String>) -> ApiError {
    ApiError {
        status,
        kind,
        message: message.into(),
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({ "error": self.kind, "message": self.message });
        (self.status, axum::Json(body)).into_response()
    }
}

fn time_describe(at: SystemTime) -> String {
    let secs = at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    stamp_format(secs)
}

impl From<Refusal> for ApiError {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::Unauthorized => api_error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "the request needs a known bearer token",
            ),
            Refusal::UserOnly(what) => api_error(
                StatusCode::FORBIDDEN,
                "user_only",
                format!("only the user token can do this: {what}"),
            ),
            Refusal::Reserved { owner, expires } => api_error(
                StatusCode::CONFLICT,
                "reserved",
                format!(
                    "the port is reserved by {owner} until {}",
                    time_describe(expires)
                ),
            ),
            Refusal::NoOwner => api_error(
                StatusCode::BAD_REQUEST,
                "no_owner",
                "the request needs an owner name (header X-Uswctl-Owner)",
            ),
            Refusal::NotOwner => api_error(
                StatusCode::FORBIDDEN,
                "not_owner",
                "only the owner of the reservation can do this",
            ),
            Refusal::NotFound(id) => api_error(
                StatusCode::NOT_FOUND,
                "not_found",
                format!("reservation {id} does not exist"),
            ),
            Refusal::StorageError(text) => {
                api_error(StatusCode::INTERNAL_SERVER_ERROR, "storage_error", text)
            }
        }
    }
}

impl From<ChangeError> for ApiError {
    fn from(e: ChangeError) -> Self {
        let (status, kind) = match &e {
            ChangeError::Locked(_) => (StatusCode::CONFLICT, "locked"),
            ChangeError::UntestedFirmware(_) => (StatusCode::LOCKED, "untested_firmware"),
            ChangeError::Settings(_) => (StatusCode::BAD_REQUEST, "settings"),
            ChangeError::ApplyFailed(_) => (StatusCode::BAD_GATEWAY, "apply_failed"),
            ChangeError::SaveFailed(_) => (StatusCode::BAD_GATEWAY, "save_failed"),
            ChangeError::NoEffect(_) => (StatusCode::UNPROCESSABLE_ENTITY, "no_effect"),
            ChangeError::Unreachable(_) => (StatusCode::SERVICE_UNAVAILABLE, "unreachable"),
            ChangeError::ReadFailed(_) => (StatusCode::BAD_GATEWAY, "read_failed"),
            ChangeError::BackupFailed(_) => (StatusCode::INTERNAL_SERVER_ERROR, "backup_failed"),
        };
        api_error(status, kind, e.to_string())
    }
}

type ApiResult<T> = Result<T, ApiError>;

fn bad_request(message: impl Into<String>) -> ApiError {
    api_error(StatusCode::BAD_REQUEST, "bad_request", message)
}

fn body_parse<T: DeserializeOwned>(body: &Bytes) -> ApiResult<T> {
    serde_json::from_slice(body).map_err(|e| bad_request(format!("request body: {e}")))
}

// The log shows the parsed body only: the raw text can hold anything a caller sent.
fn body_describe<T: std::fmt::Debug>(parsed: &ApiResult<T>) -> String {
    match parsed {
        Ok(value) => format!("{value:?}"),
        Err(_) => "(body not valid)".to_string(),
    }
}

// A daemon log line for every write, so that journald shows who changed what.
fn log_write<T>(role: &Role, what: &str, result: &ApiResult<T>) {
    match result {
        Ok(_) => tracing::info!("{role:?}: {what}: ok"),
        Err(e) => tracing::warn!("{role:?}: {what}: {} ({})", e.kind, e.message),
    }
}

fn switch_find<'a>(
    state: &'a AppState,
    name: &str,
) -> ApiResult<(&'a SwitchConfig, &'a DeviceHandle)> {
    let sw = state.cfg.switches.iter().find(|sw| sw.name == name);
    let device = state.devices.get(name);
    match (sw, device) {
        (Some(sw), Some(device)) => Ok((sw, device)),
        _ => Err(api_error(
            StatusCode::NOT_FOUND,
            "unknown_switch",
            format!("switch {name:?} does not exist"),
        )),
    }
}

fn port_check(sw: &SwitchConfig, port: u64) -> ApiResult<u8> {
    // `config_load` rejects an unknown model code, so a zero count only follows a config
    // that did not come through it, and then every port is out of range.
    let count = model_from_code(&sw.model_code).map_or(0, model_port_count);
    match u8::try_from(port) {
        Ok(idx) if (1..=count).contains(&idx) => Ok(idx),
        _ => Err(api_error(
            StatusCode::BAD_REQUEST,
            "port_range",
            format!("port {port} is outside 1 to {count} on switch {}", sw.name),
        )),
    }
}

fn port_parse(sw: &SwitchConfig, text: &str) -> ApiResult<u8> {
    let port = text.parse::<u64>().map_err(|_| {
        api_error(
            StatusCode::BAD_REQUEST,
            "port_range",
            format!("port {text:?} is not a port number"),
        )
    })?;
    port_check(sw, port)
}

fn bad_query(message: impl Into<String>) -> ApiError {
    api_error(StatusCode::BAD_REQUEST, "bad_query", message)
}

// A query that the daemon does not understand is refused: a misspelled `dry_run` must not
// turn a dry run into a write.
fn dry_run_parse(query: Option<&str>) -> ApiResult<bool> {
    let mut dry_run = None;
    for pair in query
        .unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
    {
        let Some(("dry_run", value)) = pair.split_once('=') else {
            return Err(bad_query(format!(
                "query {pair:?} is not dry_run=1, dry_run=true, dry_run=0 or dry_run=false"
            )));
        };
        if dry_run.is_some() {
            return Err(bad_query("dry_run is given more than once"));
        }
        dry_run = Some(match value {
            "1" | "true" => true,
            "0" | "false" => false,
            _ => {
                return Err(bad_query(format!(
                    "dry_run={value:?} is not 1, true, 0 or false"
                )));
            }
        });
    }
    Ok(dry_run.unwrap_or(false))
}

fn user_require(role: &Role, what: &'static str) -> ApiResult<()> {
    match role {
        Role::User => Ok(()),
        Role::Agent { .. } => Err(Refusal::UserOnly(what).into()),
    }
}

fn ttl_check(ttl_s: u64) -> ApiResult<Duration> {
    if ttl_s == 0 || ttl_s > TTL_MAX_S {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "ttl_range",
            format!("ttl_s {ttl_s} is outside 1 to {TTL_MAX_S}"),
        ));
    }
    Ok(Duration::from_secs(ttl_s))
}

async fn role_require(State(state): State<Shared>, mut request: Request, next: Next) -> Response {
    let Some(role) = role_from_headers(&state.cfg, request.headers()) else {
        return ApiError::from(Refusal::Unauthorized).into_response();
    };
    request.extensions_mut().insert(role);
    next.run(request).await
}

async fn whoami_get(Extension(role): Extension<Role>) -> axum::Json<serde_json::Value> {
    let body = match role {
        Role::User => json!({ "role": "user" }),
        Role::Agent { owner } => json!({ "role": "agent", "owner": owner }),
    };
    axum::Json(body)
}

#[derive(Serialize)]
struct SwitchSummary {
    name: String,
    model: String,
    firmware: String,
    firmware_tested: bool,
    uptime_s: f64,
    last_event: Option<Event>,
    locate: Option<Locate>,
}

async fn switches_list(State(state): State<Shared>) -> axum::Json<Vec<SwitchSummary>> {
    let mut list = Vec::new();
    for sw in &state.cfg.switches {
        let Some(device) = state.devices.get(&sw.name) else {
            continue;
        };
        let device_state = device.state_get().await;
        list.push(SwitchSummary {
            name: sw.name.clone(),
            model: sw.model_code.clone(),
            firmware: device_state.firmware,
            firmware_tested: device_state.firmware_tested,
            uptime_s: device_state.uptime_s,
            last_event: device_state.events.last().cloned(),
            locate: device_state.locate,
        });
    }
    axum::Json(list)
}

async fn switch_get(
    State(state): State<Shared>,
    Path(name): Path<String>,
) -> ApiResult<axum::Json<DeviceState>> {
    let (_, device) = switch_find(&state, &name)?;
    Ok(axum::Json(device.state_get().await))
}

// The order of the checks is fixed: a bad port never reaches the device, the safety check
// comes before the query, and a dry run skips only the reservation, because it reads and
// does not write. The body is parsed by the caller but its error counts only after the
// checks, so that a refusal does not depend on the body.
async fn port_patch_run(
    state: &AppState,
    role: &Role,
    name: &str,
    idx_text: &str,
    query: Option<&str>,
    patch: &ApiResult<PortSettingsPatch>,
) -> ApiResult<ChangeResult> {
    let (sw, device) = switch_find(state, name)?;
    let idx = port_parse(sw, idx_text)?;
    safety_check(role, sw, Target::Port(idx))?;
    let dry_run = dry_run_parse(query)?;
    if !dry_run {
        state.reservations.reservation_check(name, idx, role)?;
    }
    let patch = patch.clone()?;
    Ok(device
        .change_run(Change::Port { idx, patch }, dry_run)
        .await?)
}

async fn port_patch(
    State(state): State<Shared>,
    Extension(role): Extension<Role>,
    Path((name, idx_text)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> ApiResult<axum::Json<ChangeResult>> {
    let patch = body_parse(&body);
    let result = port_patch_run(&state, &role, &name, &idx_text, query.as_deref(), &patch).await;
    let what = format!(
        "{name} port {idx_text} change {} (query {:?})",
        body_describe(&patch),
        query.unwrap_or_default()
    );
    log_write(&role, &what, &result);
    result.map(axum::Json)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionBody {
    action: Action,
}

async fn port_action_run(
    state: &AppState,
    role: &Role,
    name: &str,
    idx_text: &str,
    request: &ApiResult<ActionBody>,
) -> ApiResult<serde_json::Value> {
    let (sw, device) = switch_find(state, name)?;
    let idx = port_parse(sw, idx_text)?;
    safety_check(role, sw, Target::Port(idx))?;
    state.reservations.reservation_check(name, idx, role)?;
    let ActionBody { action } = request.clone()?;
    let script = device.action_run(idx, action).await?;
    let cli_lines: Vec<&str> = script.lines().collect();
    Ok(json!({ "switch": name, "port": idx, "action": action, "cli_lines": cli_lines }))
}

async fn port_action(
    State(state): State<Shared>,
    Extension(role): Extension<Role>,
    Path((name, idx_text)): Path<(String, String)>,
    body: Bytes,
) -> ApiResult<axum::Json<serde_json::Value>> {
    let request = body_parse(&body);
    let result = port_action_run(&state, &role, &name, &idx_text, &request).await;
    let what = format!("{name} port {idx_text} action {}", body_describe(&request));
    log_write(&role, &what, &result);
    result.map(axum::Json)
}

async fn global_patch_run(
    state: &AppState,
    role: &Role,
    name: &str,
    query: Option<&str>,
    body: &Bytes,
) -> ApiResult<ChangeResult> {
    let (sw, device) = switch_find(state, name)?;
    safety_check(role, sw, Target::Global)?;
    let dry_run = dry_run_parse(query)?;
    let patch: GlobalSettingsPatch = body_parse(body)?;
    Ok(device.change_run(Change::Global { patch }, dry_run).await?)
}

async fn global_patch(
    State(state): State<Shared>,
    Extension(role): Extension<Role>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
    body: Bytes,
) -> ApiResult<axum::Json<ChangeResult>> {
    let result = global_patch_run(&state, &role, &name, query.as_deref(), &body).await;
    let what = format!("{name} global change ({})", query.unwrap_or_default());
    log_write(&role, &what, &result);
    result.map(axum::Json)
}

async fn reboot_post(
    State(state): State<Shared>,
    Extension(role): Extension<Role>,
    Path(name): Path<String>,
) -> ApiResult<axum::Json<serde_json::Value>> {
    let run = async {
        let (sw, device) = switch_find(&state, &name)?;
        safety_check(&role, sw, Target::Reboot)?;
        device.reboot_run().await?;
        Ok(json!({}))
    };
    let result = run.await;
    log_write(&role, &format!("{name} reboot"), &result);
    result.map(axum::Json)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocateBody {
    ports: Vec<u64>,
    duration_s: Option<u64>,
}

fn locate_duration_check(duration_s: Option<u64>) -> ApiResult<Duration> {
    let Some(duration_s) = duration_s else {
        return Ok(LOCATE_DURATION_DEFAULT);
    };
    if !(LOCATE_DURATION_MIN_S..=LOCATE_DURATION_MAX_S).contains(&duration_s) {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "duration_range",
            format!(
                "duration_s {duration_s} is outside {LOCATE_DURATION_MIN_S} to {LOCATE_DURATION_MAX_S}"
            ),
        ));
    }
    Ok(Duration::from_secs(duration_s))
}

// A locate changes only the port LEDs and the touchscreen, so it has no safety check and no
// reservation check: an agent can show the user a protected port too.
async fn locate_post_run(
    state: &AppState,
    name: &str,
    request: &ApiResult<LocateBody>,
) -> ApiResult<serde_json::Value> {
    let (sw, device) = switch_find(state, name)?;
    let request = request.clone()?;
    let ports = request
        .ports
        .iter()
        .map(|&port| port_check(sw, port))
        .collect::<ApiResult<Vec<u8>>>()?;
    // The worker checks the list too; this check answers a bad list without a wait in the
    // queue of the worker.
    if let Some(model) = model_from_code(&sw.model_code) {
        locate_ports_check(model, &ports).map_err(ChangeError::Settings)?;
    }
    let duration = locate_duration_check(request.duration_s)?;
    let (locate, script) = device.locate_run(ports, duration).await?;
    Ok(json!({
        "switch": name,
        "ports": locate.ports,
        "until": locate.until,
        "cli_lines": [script],
    }))
}

async fn locate_post(
    State(state): State<Shared>,
    Extension(role): Extension<Role>,
    Path(name): Path<String>,
    body: Bytes,
) -> ApiResult<axum::Json<serde_json::Value>> {
    let request = body_parse(&body);
    let result = locate_post_run(&state, &name, &request).await;
    let what = format!("{name} locate {}", body_describe(&request));
    log_write(&role, &what, &result);
    result.map(axum::Json)
}

async fn locate_delete(
    State(state): State<Shared>,
    Extension(role): Extension<Role>,
    Path(name): Path<String>,
) -> ApiResult<axum::Json<serde_json::Value>> {
    let run = async {
        let (_, device) = switch_find(&state, &name)?;
        let script = device.locate_off_run().await?;
        Ok(json!({ "switch": name, "cli_lines": [script] }))
    };
    let result = run.await;
    log_write(&role, &format!("{name} locate off"), &result);
    result.map(axum::Json)
}

async fn backups_get(
    State(state): State<Shared>,
    Extension(role): Extension<Role>,
    Path(name): Path<String>,
) -> ApiResult<axum::Json<Vec<String>>> {
    switch_find(&state, &name)?;
    user_require(&role, "backup list")?;
    Ok(axum::Json(state.backups.backup_list(&name)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreBody {
    stamp: String,
}

async fn restore_run(
    state: &AppState,
    role: &Role,
    name: &str,
    body: &Bytes,
) -> ApiResult<ChangeResult> {
    let (_, device) = switch_find(state, name)?;
    user_require(role, "restore")?;
    let RestoreBody { stamp } = body_parse(body)?;
    // A stamp that names no backup is a caller mistake; without this check it would reach
    // the worker and come back as a failure of this host (BackupFailed, 500).
    if !state.backups.backup_list(name).contains(&stamp) {
        return Err(api_error(
            StatusCode::NOT_FOUND,
            "unknown_backup",
            format!("switch {name} has no backup {stamp:?}"),
        ));
    }
    Ok(device.restore_run(&stamp).await?)
}

async fn restore_post(
    State(state): State<Shared>,
    Extension(role): Extension<Role>,
    Path(name): Path<String>,
    body: Bytes,
) -> ApiResult<axum::Json<ChangeResult>> {
    let result = restore_run(&state, &role, &name, &body).await;
    log_write(&role, &format!("{name} restore"), &result);
    result.map(axum::Json)
}

async fn reservations_list(State(state): State<Shared>) -> axum::Json<Vec<Reservation>> {
    axum::Json(state.reservations.reservation_list())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReservationBody {
    switch: String,
    port: u64,
    owner: Option<String>,
    ttl_s: u64,
}

// An agent reserves only under its own owner name; the user may name any owner, for
// example a script that has no session name.
fn owner_resolve(role: &Role, requested: Option<String>) -> ApiResult<String> {
    let requested = requested.filter(|o| !o.trim().is_empty());
    match role {
        Role::Agent { owner: None } => Err(Refusal::NoOwner.into()),
        Role::Agent { owner: Some(own) } => match requested {
            Some(other) if other != *own => Err(api_error(
                StatusCode::BAD_REQUEST,
                "owner_mismatch",
                format!("owner {other:?} differs from the header owner {own:?}"),
            )),
            _ => Ok(own.clone()),
        },
        Role::User => requested.ok_or_else(|| Refusal::NoOwner.into()),
    }
}

fn reservation_add_run(
    state: &AppState,
    role: &Role,
    request: &ApiResult<ReservationBody>,
) -> ApiResult<Reservation> {
    let request = request.as_ref().map_err(Clone::clone)?;
    let (sw, _) = switch_find(state, &request.switch)?;
    let port = port_check(sw, request.port)?;
    // A reservation of a protected port would let an agent block the user's own changes
    // there, so it takes the same rule as a change.
    safety_check(role, sw, Target::Port(port))?;
    let owner = owner_resolve(role, request.owner.clone())?;
    let ttl = ttl_check(request.ttl_s)?;
    Ok(state
        .reservations
        .reservation_add(&request.switch, port, &owner, ttl)?)
}

async fn reservation_add(
    State(state): State<Shared>,
    Extension(role): Extension<Role>,
    body: Bytes,
) -> ApiResult<(StatusCode, axum::Json<Reservation>)> {
    let request = body_parse(&body);
    let result = reservation_add_run(&state, &role, &request);
    log_write(
        &role,
        &format!("reserve {}", body_describe(&request)),
        &result,
    );
    result.map(|r| (StatusCode::CREATED, axum::Json(r)))
}

fn reservation_id_parse(text: &str) -> ApiResult<u64> {
    text.parse()
        .map_err(|_| bad_request(format!("reservation id {text:?} is not a number")))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtendBody {
    ttl_s: u64,
}

fn reservation_extend_run(
    state: &AppState,
    role: &Role,
    id_text: &str,
    body: &Bytes,
) -> ApiResult<Reservation> {
    let id = reservation_id_parse(id_text)?;
    let ExtendBody { ttl_s } = body_parse(body)?;
    let ttl = ttl_check(ttl_s)?;
    // The user has no owner name, so it is never the owner of a reservation.
    let owner = match role {
        Role::Agent { owner: Some(owner) } => owner,
        Role::Agent { owner: None } => return Err(Refusal::NoOwner.into()),
        Role::User => return Err(Refusal::NotOwner.into()),
    };
    Ok(state.reservations.reservation_extend(id, owner, ttl)?)
}

async fn reservation_extend(
    State(state): State<Shared>,
    Extension(role): Extension<Role>,
    Path(id_text): Path<String>,
    body: Bytes,
) -> ApiResult<axum::Json<Reservation>> {
    let result = reservation_extend_run(&state, &role, &id_text, &body);
    log_write(&role, &format!("extend reservation {id_text}"), &result);
    result.map(axum::Json)
}

async fn reservation_delete(
    State(state): State<Shared>,
    Extension(role): Extension<Role>,
    Path(id_text): Path<String>,
) -> ApiResult<axum::Json<serde_json::Value>> {
    let run = || {
        let id = reservation_id_parse(&id_text)?;
        state.reservations.reservation_release(id, &role)?;
        Ok(json!({}))
    };
    let result = run();
    log_write(&role, &format!("release reservation {id_text}"), &result);
    result.map(axum::Json)
}

async fn index_get() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        INDEX_HTML,
    )
}

async fn app_js_get() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        APP_JS,
    )
}

async fn app_css_get() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS)
}

/// The router of the daemon. Every `/api` route needs a token; the role check runs before
/// any path, query or body parse, so that a caller without a token learns nothing else.
/// The web files need no token, because the page asks for the token itself.
pub fn router_build(state: AppState) -> Router {
    let shared: Shared = Arc::new(state);
    let api = Router::new()
        .route("/api/whoami", get(whoami_get))
        .route("/api/switches", get(switches_list))
        .route("/api/switches/{sw}", get(switch_get))
        .route("/api/switches/{sw}/ports/{idx}", patch(port_patch))
        .route("/api/switches/{sw}/ports/{idx}/actions", post(port_action))
        .route("/api/switches/{sw}/global", patch(global_patch))
        .route("/api/switches/{sw}/reboot", post(reboot_post))
        .route(
            "/api/switches/{sw}/locate",
            post(locate_post).delete(locate_delete),
        )
        .route("/api/switches/{sw}/backups", get(backups_get))
        .route("/api/switches/{sw}/restore", post(restore_post))
        .route(
            "/api/reservations",
            get(reservations_list).post(reservation_add),
        )
        .route("/api/reservations/{id}/extend", post(reservation_extend))
        .route("/api/reservations/{id}", delete(reservation_delete))
        .route_layer(from_fn_with_state(shared.clone(), role_require));
    Router::new()
        .route("/", get(index_get))
        .route("/app.js", get(app_js_get))
        .route("/app.css", get(app_css_get))
        .merge(api)
        .with_state(shared)
}
