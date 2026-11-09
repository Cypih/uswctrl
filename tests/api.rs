// Integration tests for the HTTP API: roles, safety rules, reservations and status codes.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use uswctl::api::router_build;
use uswctl::backup::BackupStore;
use uswctl::fake::{FIRMWARE, LOCK_CHECK_PREFIX, Sim, XG10_CFG};
use uswctl::transport::FakeTransport;

use common::{AGENT, USER, state_build};

struct Env {
    router: Router,
    fake: Arc<FakeTransport>,
    sim: Arc<Mutex<Sim>>,
    backups: BackupStore,
    _dir: tempfile::TempDir,
}

async fn env_build(slow: bool) -> Env {
    let common::StateEnv {
        state,
        fake,
        sim,
        backups,
        dir,
    } = state_build(slow).await;
    Env {
        router: router_build(state),
        fake,
        sim,
        backups,
        _dir: dir,
    }
}

enum Caller<'a> {
    Nobody,
    User,
    Agent(Option<&'a str>),
}

fn request_build(method: Method, uri: &str, caller: Caller, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    match caller {
        Caller::Nobody => {}
        Caller::User => {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {USER}"));
        }
        Caller::Agent(owner) => {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {AGENT}"));
            if let Some(owner) = owner {
                builder = builder.header("X-Uswctl-Owner", owner);
            }
        }
    }
    match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

async fn call(router: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, value)
}

async fn reserve(env: &Env, owner: &str, port: u8) -> Value {
    let (status, body) = call(
        &env.router,
        request_build(
            Method::POST,
            "/api/reservations",
            Caller::Agent(Some(owner)),
            Some(json!({"switch": "xg10", "port": port, "ttl_s": 600})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

fn name_patch(name: &str) -> Value {
    json!({ "name": name })
}

fn file_puts(env: &Env) -> Vec<String> {
    env.fake.file_puts_get()
}

#[tokio::test(start_paused = true)]
async fn no_token_401() {
    let env = env_build(false).await;
    let (status, body) = call(
        &env.router,
        request_build(Method::GET, "/api/switches", Caller::Nobody, None),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "unauthorized");
    assert!(body["message"].is_string(), "{body}");

    let request = Request::builder()
        .uri("/api/switches")
        .header(header::AUTHORIZATION, "Bearer wrong")
        .body(Body::empty())
        .unwrap();
    let (status, _) = call(&env.router, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test(start_paused = true)]
async fn no_token_401_before_bad_body() {
    let env = env_build(false).await;
    let request = Request::builder()
        .method(Method::PATCH)
        .uri("/api/switches/nosuch/ports/99")
        .body(Body::from("not json"))
        .unwrap();
    let (status, _) = call(&env.router, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test(start_paused = true)]
async fn agent_patch_protected_403() {
    let env = env_build(false).await;
    let (status, body) = call(
        &env.router,
        request_build(
            Method::PATCH,
            "/api/switches/xg10/ports/11",
            Caller::Agent(Some("a")),
            Some(name_patch("x")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "user_only");
    let (status, body) = call(
        &env.router,
        request_build(
            Method::POST,
            "/api/switches/xg10/ports/11/actions",
            Caller::Agent(Some("a")),
            Some(json!({"action": "poe_cycle"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(file_puts(&env).is_empty());
}

#[tokio::test(start_paused = true)]
async fn agent_patch_reserved_by_other_409() {
    let env = env_build(false).await;
    reserve(&env, "a", 3).await;
    let (status, body) = call(
        &env.router,
        request_build(
            Method::PATCH,
            "/api/switches/xg10/ports/3",
            Caller::Agent(Some("b")),
            Some(name_patch("x")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "reserved");
    assert!(body["message"].as_str().unwrap().contains("a"), "{body}");

    let (status, _) = call(
        &env.router,
        request_build(
            Method::POST,
            "/api/switches/xg10/ports/3/actions",
            Caller::Agent(None),
            Some(json!({"action": "link_down"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(file_puts(&env).is_empty());
}

#[tokio::test(start_paused = true)]
async fn dry_run_on_reserved_port_allowed() {
    let env = env_build(false).await;
    reserve(&env, "a", 3).await;
    let (status, body) = call(
        &env.router,
        request_build(
            Method::PATCH,
            "/api/switches/xg10/ports/3?dry_run=1",
            Caller::Agent(Some("b")),
            Some(name_patch("DUT 3")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test(start_paused = true)]
async fn patch_port_99_400() {
    let env = env_build(false).await;
    for uri in [
        "/api/switches/xg10/ports/99",
        "/api/switches/xg10/ports/0",
        "/api/switches/xg10/ports/300",
        "/api/switches/xg10/ports/abc",
    ] {
        let (status, body) = call(
            &env.router,
            request_build(Method::PATCH, uri, Caller::User, Some(name_patch("x"))),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
        assert_eq!(body["error"], "port_range", "{uri}: {body}");
    }
    assert!(
        !env.fake
            .scripts_get()
            .iter()
            .any(|s| s.starts_with(LOCK_CHECK_PREFIX))
    );
}

#[tokio::test(start_paused = true)]
async fn unknown_switch_404() {
    let env = env_build(false).await;
    let (status, body) = call(
        &env.router,
        request_build(Method::GET, "/api/switches/nosuch", Caller::User, None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "unknown_switch");
}

#[tokio::test(start_paused = true)]
async fn bad_body_400() {
    let env = env_build(false).await;
    for body in [
        Body::from("not json"),
        Body::from(json!({"poe": "sometimes"}).to_string()),
        Body::from(json!({"nmae": "typo"}).to_string()),
    ] {
        let request = Request::builder()
            .method(Method::PATCH)
            .uri("/api/switches/xg10/ports/3")
            .header(header::AUTHORIZATION, format!("Bearer {USER}"))
            .body(body)
            .unwrap();
        let (status, value) = call(&env.router, request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
        assert_eq!(value["error"], "bad_request");
    }
    let (status, body) = call(
        &env.router,
        request_build(
            Method::PATCH,
            "/api/switches/xg10/global?dry_run=1",
            Caller::User,
            Some(json!({"stp_prio": 4096})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test(start_paused = true)]
async fn settings_error_400() {
    let env = env_build(false).await;
    let (status, body) = call(
        &env.router,
        request_build(
            Method::PATCH,
            "/api/switches/xg10/ports/3?dry_run=1",
            Caller::User,
            Some(json!({"native_vlan": 3999})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "settings");
}

#[tokio::test(start_paused = true)]
async fn dry_run_returns_keys_without_put() {
    let env = env_build(false).await;
    let (status, body) = call(
        &env.router,
        request_build(
            Method::PATCH,
            "/api/switches/xg10/ports/3?dry_run=1",
            Caller::Agent(Some("a")),
            Some(name_patch("DUT 3")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let keys = body["keys"].as_array().unwrap();
    assert!(!keys.is_empty(), "{body}");
    assert!(body.to_string().contains("DUT 3"), "{body}");
    assert_eq!(body["readback_ok"], false);
    assert!(file_puts(&env).is_empty(), "{:?}", file_puts(&env));
}

#[tokio::test(start_paused = true)]
async fn no_effect_422() {
    let env = env_build(false).await;
    let (status, body) = call(
        &env.router,
        request_build(
            Method::PATCH,
            "/api/switches/xg10/ports/3",
            Caller::Agent(Some("a")),
            Some(name_patch("DUT 3")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"], "no_effect");
    assert!(
        file_puts(&env).contains(&"/tmp/system.cfg".to_string()),
        "{:?}",
        file_puts(&env)
    );
}

#[tokio::test(start_paused = true)]
async fn locked_409_and_untested_423() {
    let env = env_build(false).await;
    env.sim.lock().unwrap().lock = "other-session host-b".to_string();
    let (status, body) = call(
        &env.router,
        request_build(
            Method::PATCH,
            "/api/switches/xg10/ports/3",
            Caller::User,
            Some(name_patch("DUT 3")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "locked");

    {
        let mut sim = env.sim.lock().unwrap();
        sim.lock = String::new();
        sim.firmware = "7.0.0.1".to_string();
    }
    let (status, body) = call(
        &env.router,
        request_build(
            Method::PATCH,
            "/api/switches/xg10/ports/3",
            Caller::User,
            Some(name_patch("DUT 3")),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::LOCKED, "{body}");
    assert_eq!(body["error"], "untested_firmware");
}

#[tokio::test(start_paused = true)]
async fn global_by_agent_403() {
    let env = env_build(false).await;
    let (status, body) = call(
        &env.router,
        request_build(
            Method::PATCH,
            "/api/switches/xg10/global?dry_run=1",
            Caller::Agent(Some("a")),
            Some(json!({"stp_priority": 4096})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "user_only");
}

#[tokio::test(start_paused = true)]
async fn global_by_user_ok() {
    let env = env_build(false).await;
    let (status, body) = call(
        &env.router,
        request_build(
            Method::PATCH,
            "/api/switches/xg10/global?dry_run=1",
            Caller::User,
            Some(json!({"stp_priority": 4096})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body["keys"].as_array().unwrap().is_empty(), "{body}");
    assert!(file_puts(&env).is_empty());
}

#[tokio::test(start_paused = true)]
async fn reboot_by_agent_403() {
    let env = env_build(false).await;
    let (status, body) = call(
        &env.router,
        request_build(
            Method::POST,
            "/api/switches/xg10/reboot",
            Caller::Agent(Some("a")),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(!env.fake.scripts_get().iter().any(|s| s.contains("reboot")));
}

#[tokio::test(start_paused = true)]
async fn restore_by_agent_403() {
    let env = env_build(false).await;
    let (status, body) = call(
        &env.router,
        request_build(
            Method::POST,
            "/api/switches/xg10/restore",
            Caller::Agent(Some("a")),
            Some(json!({"stamp": "20261103T120000Z"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "user_only");
    let (status, _) = call(
        &env.router,
        request_build(
            Method::GET,
            "/api/switches/xg10/backups",
            Caller::Agent(Some("a")),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test(start_paused = true)]
async fn backups_and_restore_by_user() {
    let env = env_build(false).await;
    let saved = env.backups.backup_save("xg10", XG10_CFG, "").unwrap();
    let stamp = saved.file_name().unwrap().to_str().unwrap().to_string();
    let (status, body) = call(
        &env.router,
        request_build(
            Method::GET,
            "/api/switches/xg10/backups",
            Caller::User,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([stamp]));

    let (status, body) = call(
        &env.router,
        request_build(
            Method::POST,
            "/api/switches/xg10/restore",
            Caller::User,
            Some(json!({"stamp": "20200101T000000Z"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], "unknown_backup");

    // The restore reaches the worker; the fake switch shows no overlay in the running
    // config afterwards, so the answer is a device-side result, not a refusal.
    let (status, body) = call(
        &env.router,
        request_build(
            Method::POST,
            "/api/switches/xg10/restore",
            Caller::User,
            Some(json!({ "stamp": stamp })),
        ),
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::UNPROCESSABLE_ENTITY,
        "{status} {body}"
    );
    assert!(file_puts(&env).contains(&"/tmp/system.cfg".to_string()));
}

#[tokio::test(start_paused = true)]
async fn whoami_agent() {
    let env = env_build(false).await;
    let (status, body) = call(
        &env.router,
        request_build(
            Method::GET,
            "/api/whoami",
            Caller::Agent(Some("agent-01")),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"role": "agent", "owner": "agent-01"}));
    let (_, body) = call(
        &env.router,
        request_build(Method::GET, "/api/whoami", Caller::User, None),
    )
    .await;
    assert_eq!(body, json!({"role": "user"}));
}

#[tokio::test(start_paused = true)]
async fn switches_list_and_state() {
    let env = env_build(false).await;
    let (status, body) = call(
        &env.router,
        request_build(Method::GET, "/api/switches", Caller::Agent(None), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let list = body.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["name"], "xg10");
    assert_eq!(list[0]["model"], "USWED77");
    assert_eq!(list[0]["firmware"], FIRMWARE);
    assert_eq!(list[0]["firmware_tested"], true);
    assert!(list[0]["uptime_s"].as_f64().unwrap() > 0.0, "{body}");
    assert!(list[0].get("last_event").is_some(), "{body}");

    let (status, body) = call(
        &env.router,
        request_build(Method::GET, "/api/switches/xg10", Caller::Agent(None), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ports"].as_array().unwrap().len(), 12, "{body}");
    assert_eq!(body["ports"][10]["protected"], true);
}

#[tokio::test(start_paused = true)]
async fn action_link_down_ok() {
    let env = env_build(false).await;
    let (status, body) = call(
        &env.router,
        request_build(
            Method::POST,
            "/api/switches/xg10/ports/1/actions",
            Caller::Agent(Some("a")),
            Some(json!({"action": "link_down"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["action"], "link_down");
    assert!(
        body["cli_lines"]
            .as_array()
            .unwrap()
            .contains(&json!("interface te1")),
        "{body}"
    );
    assert_eq!(file_puts(&env), vec!["/tmp/uswctl-action.scr".to_string()]);
    let (status, body) = call(
        &env.router,
        request_build(
            Method::POST,
            "/api/switches/xg10/ports/1/actions",
            Caller::Agent(Some("a")),
            Some(json!({"action": "explode"})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test(start_paused = true)]
async fn agent_reserve_protected_403() {
    let env = env_build(false).await;
    let (status, body) = call(
        &env.router,
        request_build(
            Method::POST,
            "/api/reservations",
            Caller::Agent(Some("a")),
            Some(json!({"switch": "xg10", "port": 11, "ttl_s": 60})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "user_only");
    let (status, body) = call(
        &env.router,
        request_build(Method::GET, "/api/reservations", Caller::User, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]));
    let (status, body) = call(
        &env.router,
        request_build(
            Method::POST,
            "/api/reservations",
            Caller::User,
            Some(json!({"switch": "xg10", "port": 11, "owner": "lab", "ttl_s": 60})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test(start_paused = true)]
async fn reservation_rules() {
    let env = env_build(false).await;
    let post =
        |caller, body: Value| request_build(Method::POST, "/api/reservations", caller, Some(body));
    let (status, body) = call(
        &env.router,
        post(
            Caller::Agent(None),
            json!({"switch": "xg10", "port": 3, "ttl_s": 60}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "no_owner");

    let (status, body) = call(
        &env.router,
        post(
            Caller::Agent(Some("a")),
            json!({"switch": "xg10", "port": 3, "owner": "b", "ttl_s": 60}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "owner_mismatch");

    for ttl in [0, 86_401] {
        let (status, body) = call(
            &env.router,
            post(
                Caller::Agent(Some("a")),
                json!({"switch": "xg10", "port": 3, "ttl_s": ttl}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "ttl_range");
    }

    let (status, _) = call(
        &env.router,
        post(
            Caller::Agent(Some("a")),
            json!({"switch": "xg10", "port": 13, "ttl_s": 60}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        &env.router,
        post(
            Caller::Agent(Some("a")),
            json!({"switch": "nosuch", "port": 3, "ttl_s": 60}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = call(
        &env.router,
        post(
            Caller::User,
            json!({"switch": "xg10", "port": 4, "owner": "script", "ttl_s": 60}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["owner"], "script");

    let created = reserve(&env, "a", 3).await;
    let id = created["id"].as_u64().unwrap();
    let (status, body) = call(
        &env.router,
        post(
            Caller::Agent(Some("b")),
            json!({"switch": "xg10", "port": 3, "ttl_s": 60}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let extend_uri = format!("/api/reservations/{id}/extend");
    let (status, _) = call(
        &env.router,
        request_build(
            Method::POST,
            &extend_uri,
            Caller::Agent(Some("b")),
            Some(json!({"ttl_s": 60})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = call(
        &env.router,
        request_build(
            Method::POST,
            &extend_uri,
            Caller::User,
            Some(json!({"ttl_s": 60})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], "not_owner");
    let (status, body) = call(
        &env.router,
        request_build(
            Method::POST,
            &extend_uri,
            Caller::Agent(Some("a")),
            Some(json!({"ttl_s": 7200})),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], id);

    let (status, body) = call(
        &env.router,
        request_build(Method::GET, "/api/reservations", Caller::Agent(None), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 2, "{body}");

    let delete_uri = format!("/api/reservations/{id}");
    let (status, _) = call(
        &env.router,
        request_build(Method::DELETE, &delete_uri, Caller::Agent(Some("b")), None),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = call(
        &env.router,
        request_build(Method::DELETE, &delete_uri, Caller::Agent(None), None),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"], "no_owner");
    let (status, _) = call(
        &env.router,
        request_build(Method::DELETE, &delete_uri, Caller::User, None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(
        &env.router,
        request_build(Method::DELETE, &delete_uri, Caller::User, None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"], "not_found");
}

#[tokio::test(start_paused = true)]
async fn ambiguous_dry_run_400() {
    let env = env_build(false).await;
    for query in [
        "dryrun=1",
        "dry_run",
        "DRY_RUN=1",
        "dry_run=yes",
        "dry_run=True",
        "dry_run=1&dry_run=0",
        "dry_run=1&x=2",
    ] {
        let (status, body) = call(
            &env.router,
            request_build(
                Method::PATCH,
                &format!("/api/switches/xg10/ports/3?{query}"),
                Caller::User,
                Some(name_patch("DUT 3")),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert_eq!(body["error"], "bad_query", "{query}: {body}");
        let (status, _) = call(
            &env.router,
            request_build(
                Method::PATCH,
                &format!("/api/switches/xg10/global?{query}"),
                Caller::User,
                Some(json!({"stp_priority": 4096})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
    }
    assert!(file_puts(&env).is_empty(), "{:?}", file_puts(&env));
    for query in ["dry_run=true", "dry_run=1", "", "dry_run=0&"] {
        let uri = format!("/api/switches/xg10/ports/3?{query}");
        let (status, body) = call(
            &env.router,
            request_build(Method::PATCH, &uri, Caller::User, Some(json!({}))),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{query}: {body}");
    }
}

#[tokio::test(start_paused = true)]
async fn agent_dry_run_protected_403() {
    let env = env_build(false).await;
    for (uri, body) in [
        ("/api/switches/xg10/ports/11?dry_run=1", name_patch("x")),
        ("/api/switches/xg10/ports/11?dryrun=1", name_patch("x")),
        ("/api/switches/xg10/ports/11?dry_run=x", name_patch("x")),
        (
            "/api/switches/xg10/global?dry_run",
            json!({"stp_priority": 4096}),
        ),
        (
            "/api/switches/xg10/global?dryrun=1",
            json!({"stp_priority": 4096}),
        ),
    ] {
        let (status, value) = call(
            &env.router,
            request_build(Method::PATCH, uri, Caller::Agent(Some("a")), Some(body)),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {value}");
        assert_eq!(value["error"], "user_only", "{uri}: {value}");
    }
}

#[tokio::test(start_paused = true)]
async fn index_served_without_token() {
    let env = env_build(false).await;
    for (uri, content_type) in [
        ("/", "text/html"),
        ("/app.js", "text/javascript"),
        ("/app.css", "text/css"),
    ] {
        let response = env
            .router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        let value = response.headers()[header::CONTENT_TYPE].to_str().unwrap();
        assert!(value.starts_with(content_type), "{uri}: {value}");
    }
}

#[tokio::test(start_paused = true)]
async fn index_has_app_root() {
    let env = env_build(false).await;
    let response = env
        .router
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(html.contains(r#"id="app""#), "{html}");
}

#[tokio::test(start_paused = true)]
async fn concurrent_patches_both_answer() {
    let env = env_build(true).await;
    let (a, b) = tokio::join!(
        call(
            &env.router,
            request_build(
                Method::PATCH,
                "/api/switches/xg10/ports/3",
                Caller::Agent(Some("a")),
                Some(name_patch("DUT A")),
            ),
        ),
        call(
            &env.router,
            request_build(
                Method::PATCH,
                "/api/switches/xg10/ports/5",
                Caller::Agent(Some("b")),
                Some(name_patch("DUT B")),
            ),
        ),
    );
    assert_eq!(a.0, StatusCode::UNPROCESSABLE_ENTITY, "{}", a.1);
    assert_eq!(b.0, StatusCode::UNPROCESSABLE_ENTITY, "{}", b.1);

    let scripts = env.fake.scripts_get();
    let locks: Vec<usize> = scripts
        .iter()
        .enumerate()
        .filter(|(_, s)| s.starts_with(LOCK_CHECK_PREFIX))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(locks.len(), 2, "{scripts:#?}");
    for part in [&scripts[locks[0]..locks[1]], &scripts[locks[1]..]] {
        assert_eq!(
            part.iter()
                .filter(|s| *s == "#file_put /tmp/system.cfg")
                .count(),
            1,
            "{scripts:#?}"
        );
        assert!(
            part.iter().any(|s| s.contains("show running-config")),
            "{scripts:#?}"
        );
    }
}

async fn locate_post(env: &Env, caller: Caller<'_>, body: Value) -> (StatusCode, Value) {
    call(
        &env.router,
        request_build(
            Method::POST,
            "/api/switches/xg10/locate",
            caller,
            Some(body),
        ),
    )
    .await
}

fn locate_scripts(env: &Env) -> Vec<String> {
    env.fake
        .scripts_get()
        .into_iter()
        .filter(|s| s.contains("port-locate"))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn agent_locates_protected_and_reserved_ports() {
    let env = env_build(false).await;
    reserve(&env, "other", 3).await;
    let (status, body) = locate_post(
        &env,
        Caller::Agent(None),
        json!({ "ports": [11, 3], "duration_s": 60 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["switch"], "xg10");
    assert_eq!(body["ports"], json!([3, 11]));
    assert!(body["until"]["secs_since_epoch"].is_u64(), "{body}");
    assert_eq!(
        body["cli_lines"],
        json!(["syswrapper.sh set-port-locate 0 404"])
    );
    assert_eq!(
        locate_scripts(&env),
        vec!["syswrapper.sh set-port-locate 0 404"]
    );

    let (status, body) = call(
        &env.router,
        request_build(Method::GET, "/api/switches/xg10", Caller::Agent(None), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["locate"]["ports"], json!([3, 11]), "{body}");
    assert!(
        body["locate"]["until"]["secs_since_epoch"].is_u64(),
        "{body}"
    );

    let (status, body) = call(
        &env.router,
        request_build(Method::GET, "/api/switches", Caller::Agent(None), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["locate"]["ports"], json!([3, 11]), "{body}");
}

#[tokio::test(start_paused = true)]
async fn locate_default_duration_and_delete() {
    let env = env_build(false).await;
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (status, body) = locate_post(&env, Caller::User, json!({ "ports": [1] })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let until = body["until"]["secs_since_epoch"].as_u64().unwrap();
    assert!((before + 299..=before + 301).contains(&until), "{body}");

    let (status, body) = call(
        &env.router,
        request_build(
            Method::DELETE,
            "/api/switches/xg10/locate",
            Caller::Agent(None),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["cli_lines"],
        json!(["syswrapper.sh unset-port-locate"])
    );
    let (_, body) = call(
        &env.router,
        request_build(Method::GET, "/api/switches/xg10", Caller::Agent(None), None),
    )
    .await;
    assert_eq!(body["locate"], Value::Null, "{body}");
}

#[tokio::test(start_paused = true)]
async fn locate_bad_ports_and_duration_400() {
    let env = env_build(false).await;
    for (body, kind) in [
        (json!({ "ports": [13] }), "port_range"),
        (json!({ "ports": [0] }), "port_range"),
        (json!({ "ports": [] }), "settings"),
        (json!({ "ports": [2, 2] }), "settings"),
        (json!({ "ports": [1], "duration_s": 9 }), "duration_range"),
        (
            json!({ "ports": [1], "duration_s": 3601 }),
            "duration_range",
        ),
        (
            json!({ "ports": [1], "duration_s": 60, "colour": "red" }),
            "bad_request",
        ),
        (json!({ "duration_s": 60 }), "bad_request"),
    ] {
        let (status, answer) = locate_post(&env, Caller::Agent(None), body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {answer}");
        assert_eq!(answer["error"], kind, "{body} -> {answer}");
    }
    assert!(locate_scripts(&env).is_empty());
}

#[tokio::test(start_paused = true)]
async fn locate_role_switch_and_lock_checks() {
    let env = env_build(false).await;
    let (status, _) = locate_post(&env, Caller::Nobody, json!({ "ports": [1] })).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call(
        &env.router,
        request_build(
            Method::DELETE,
            "/api/switches/xg10/locate",
            Caller::Nobody,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = call(
        &env.router,
        request_build(
            Method::POST,
            "/api/switches/nope/locate",
            Caller::User,
            Some(json!({ "ports": [1] })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, _) = call(
        &env.router,
        request_build(
            Method::DELETE,
            "/api/switches/nope/locate",
            Caller::User,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    env.sim.lock().unwrap().lock = "other-session host-b".to_string();
    let (status, body) = locate_post(&env, Caller::User, json!({ "ports": [1] })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "locked");
    assert!(locate_scripts(&env).is_empty());

    // A stop only gives the LEDs back, so a foreign lock does not refuse it.
    let (status, body) = call(
        &env.router,
        request_build(
            Method::DELETE,
            "/api/switches/xg10/locate",
            Caller::Agent(None),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        locate_scripts(&env),
        vec!["syswrapper.sh unset-port-locate"]
    );
}
