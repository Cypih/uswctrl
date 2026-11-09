// Integration tests for config loading, roles from headers, and the safety rules.

use std::fs;
use std::os::unix::fs::PermissionsExt;

use axum::http::{HeaderMap, HeaderValue, header::AUTHORIZATION};
use uswctl::auth::{Refusal, Role, Target, role_from_headers, safety_check};
use uswctl::config::{Config, SwitchConfig, config_load};

const EXAMPLE_TOML: &str = include_str!("../config.example.toml");
const AGENT_TOKEN: &str = "agent-secret";
const USER_TOKEN: &str = "user-secret";

fn promax16() -> SwitchConfig {
    SwitchConfig {
        name: "promax16".to_string(),
        host: "192.0.2.11".to_string(),
        model_code: "USPM16P".to_string(),
        protected_ports: vec![14, 17],
        tested_firmware: vec!["7.5.15.17146".to_string()],
        lock_token: "uswctld".to_string(),
    }
}

fn xg10() -> SwitchConfig {
    SwitchConfig {
        name: "xg10".to_string(),
        host: "192.0.2.107".to_string(),
        model_code: "USWED77".to_string(),
        protected_ports: vec![11],
        tested_firmware: vec!["7.5.15.17146".to_string()],
        lock_token: "uswctld".to_string(),
    }
}

fn cfg() -> Config {
    Config {
        listen: "127.0.0.1:8650".parse().unwrap(),
        agent_token: AGENT_TOKEN.to_string(),
        user_token: USER_TOKEN.to_string(),
        ssh_user: "admin".to_string(),
        ssh_key: "/home/user/.ssh/id_rsa".into(),
        lock_path: Some("/tmp/agent-lock".to_string()),
        data_dir: "/tmp/uswctl-auth-test-data".into(),
        poll_s: 5,
        overlay_check_s: 30,
        switches: vec![promax16(), xg10()],
    }
}

fn headers_build(token: &str, owner: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    if let Some(owner) = owner {
        headers.insert("X-Uswctl-Owner", HeaderValue::from_str(owner).unwrap());
    }
    headers
}

fn config_file_write(dir: &std::path::Path, text: &str, mode: u32) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    fs::write(&path, text).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    path
}

#[test]
fn agent_token_cannot_touch_protected_port() {
    let role = Role::Agent { owner: None };
    assert_eq!(
        safety_check(&role, &promax16(), Target::Port(14)),
        Err(Refusal::UserOnly("protected port"))
    );
}

#[test]
fn agent_can_touch_unprotected_port() {
    let role = Role::Agent { owner: None };
    assert_eq!(safety_check(&role, &promax16(), Target::Port(1)), Ok(()));
}

#[test]
fn agent_cannot_global() {
    let role = Role::Agent { owner: None };
    assert_eq!(
        safety_check(&role, &promax16(), Target::Global),
        Err(Refusal::UserOnly("switch-wide setting"))
    );
}

#[test]
fn agent_cannot_reboot() {
    let role = Role::Agent {
        owner: Some("script-1".to_string()),
    };
    assert_eq!(
        safety_check(&role, &xg10(), Target::Reboot),
        Err(Refusal::UserOnly("reboot"))
    );
}

#[test]
fn user_can_global() {
    assert_eq!(
        safety_check(&Role::User, &promax16(), Target::Global),
        Ok(())
    );
}

#[test]
fn user_can_protected_port() {
    assert_eq!(
        safety_check(&Role::User, &promax16(), Target::Port(14)),
        Ok(())
    );
}

#[test]
fn agent_token_is_not_user() {
    let headers = headers_build(AGENT_TOKEN, None);
    assert_eq!(
        role_from_headers(&cfg(), &headers),
        Some(Role::Agent { owner: None })
    );
}

#[test]
fn user_token_gives_user_role() {
    let headers = headers_build(USER_TOKEN, None);
    assert_eq!(role_from_headers(&cfg(), &headers), Some(Role::User));
}

#[test]
fn agent_owner_header_is_trimmed() {
    let headers = headers_build(AGENT_TOKEN, Some("  script-1  "));
    assert_eq!(
        role_from_headers(&cfg(), &headers),
        Some(Role::Agent {
            owner: Some("script-1".to_string())
        })
    );
}

#[test]
fn agent_empty_owner_header_is_none() {
    let headers = headers_build(AGENT_TOKEN, Some("   "));
    assert_eq!(
        role_from_headers(&cfg(), &headers),
        Some(Role::Agent { owner: None })
    );
}

#[test]
fn unknown_token_is_no_role() {
    let headers = headers_build("not-a-token", None);
    assert_eq!(role_from_headers(&cfg(), &headers), None);
}

#[test]
fn missing_header_is_no_role() {
    assert_eq!(role_from_headers(&cfg(), &HeaderMap::new()), None);
}

#[test]
fn config_rejects_mode_644() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_file_write(dir.path(), EXAMPLE_TOML, 0o644);
    let err = config_load(&path).unwrap_err();
    assert!(err.to_string().contains("644"), "error was: {err}");
}

#[test]
fn config_rejects_equal_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let text = EXAMPLE_TOML
        .replace("CHANGE-ME-agent", "same-token")
        .replace("CHANGE-ME-user", "same-token");
    let path = config_file_write(dir.path(), &text, 0o600);
    let err = config_load(&path).unwrap_err();
    assert!(err.to_string().contains("differ"), "error was: {err}");
}

#[test]
fn config_rejects_zero_poll_s() {
    let dir = tempfile::tempdir().unwrap();
    let text = EXAMPLE_TOML.replace("poll_s = 5", "poll_s = 0");
    let path = config_file_write(dir.path(), &text, 0o600);
    let err = config_load(&path).unwrap_err();
    assert!(err.to_string().contains("poll_s"), "error was: {err}");
}

#[test]
fn config_rejects_unknown_model_code() {
    let dir = tempfile::tempdir().unwrap();
    let text = EXAMPLE_TOML.replace("USWED77", "NOT-A-MODEL");
    let path = config_file_write(dir.path(), &text, 0o600);
    let err = config_load(&path).unwrap_err();
    assert!(err.to_string().contains("NOT-A-MODEL"), "error was: {err}");
}

#[test]
fn config_rejects_protected_port_out_of_range() {
    let dir = tempfile::tempdir().unwrap();
    let text = EXAMPLE_TOML.replace("protected_ports = [11]", "protected_ports = [13]");
    let path = config_file_write(dir.path(), &text, 0o600);
    let err = config_load(&path).unwrap_err();
    assert!(err.to_string().contains("13"), "error was: {err}");
}

// The example ships "~/.local/share/uswctl" as `data_dir`, and `config_load` creates it if
// missing. The test swaps it for a tempdir so running the suite does not create a directory
// in the real $HOME of whoever runs `cargo test`.
#[test]
fn example_config_parses() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let text = EXAMPLE_TOML.replace(
        "data_dir = \"~/.local/share/uswctl\"",
        &format!("data_dir = {:?}", data_dir.display().to_string()),
    );
    let path = config_file_write(dir.path(), &text, 0o600);
    let cfg = config_load(&path).unwrap();
    assert_eq!(cfg.listen, "127.0.0.1:8650".parse().unwrap());
    assert_eq!(cfg.switches.len(), 2);
    assert_eq!(cfg.switches[0].name, "promax16");
    assert_eq!(cfg.switches[1].name, "xg10");
    assert!(cfg.switches[0].tested_firmware.is_empty());
    assert_eq!(cfg.switches[1].tested_firmware, vec!["7.5.15.17146"]);
    for sw in &cfg.switches {
        assert_eq!(sw.lock_token, "uswctld-own-lock-allowance");
    }
    assert!(data_dir.is_dir());
    let mode = fs::metadata(&data_dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
}

fn config_err_get(text: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = config_file_write(dir.path(), text, 0o600);
    config_load(&path).unwrap_err().to_string()
}

fn config_ok_get(text: &str) -> Config {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let text = text.replace(
        "data_dir = \"~/.local/share/uswctl\"",
        &format!("data_dir = {:?}", data_dir.display().to_string()),
    );
    let path = config_file_write(dir.path(), &text, 0o600);
    config_load(&path).unwrap()
}

#[test]
fn example_config_ssh_user_and_lock_path() {
    let cfg = config_ok_get(EXAMPLE_TOML);
    assert_eq!(cfg.ssh_user, "CHANGE-ME-ssh-user");
    assert_eq!(cfg.lock_path.as_deref(), Some("/tmp/agent-lock"));
}

#[test]
fn config_requires_ssh_user() {
    let text = EXAMPLE_TOML.replace("ssh_user = \"CHANGE-ME-ssh-user\"\n", "");
    let err = config_err_get(&text);
    assert!(err.contains("parse"), "error was: {err}");
}

#[test]
fn config_rejects_bad_ssh_user() {
    for user in ["", "-oProxyCommand=x", "a b", "user@host"] {
        let text = EXAMPLE_TOML.replace("CHANGE-ME-ssh-user", user);
        let err = config_err_get(&text);
        assert!(err.contains("ssh_user"), "user {user:?}: error was: {err}");
    }
}

#[test]
fn config_lock_path_is_optional() {
    let text = EXAMPLE_TOML
        .replace("lock_path = \"/tmp/agent-lock\"\n", "")
        .replace("lock_token = \"uswctld-own-lock-allowance\"\n", "");
    let cfg = config_ok_get(&text);
    assert_eq!(cfg.lock_path, None);
    for sw in &cfg.switches {
        assert_eq!(sw.lock_token, "");
    }
}

#[test]
fn config_rejects_bad_lock_path() {
    for lock_path in [
        "tmp/lock",
        "/tmp/",
        "/tmp/a lock",
        "/tmp/x;rm",
        "/tmp/$(id)",
        "",
    ] {
        let text = EXAMPLE_TOML.replace("/tmp/agent-lock", lock_path);
        let err = config_err_get(&text);
        assert!(
            err.contains("lock_path"),
            "path {lock_path:?}: error was: {err}"
        );
    }
}

#[test]
fn config_rejects_unknown_keys() {
    let text = EXAMPLE_TOML.replace("lock_path = ", "lockpath = ");
    let err = config_err_get(&text);
    assert!(err.contains("parse"), "error was: {err}");
    let text = format!("{EXAMPLE_TOML}lock_path = \"/tmp/agent-lock\"\n");
    let err = config_err_get(&text);
    assert!(err.contains("parse"), "error was: {err}");
}
