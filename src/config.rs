// The daemon's TOML config file: its shape, its validation, and loading it from disk.

use std::collections::HashSet;
use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;

use crate::model::{model_from_code, model_port_count};

const CONFIG_MODE: u32 = 0o600;
const DATA_DIR_MODE: u32 = 0o700;

// An unknown key is an error, so that a misspelled optional key such as `lock_path` cannot
// turn its check off without a word.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwitchConfig {
    pub name: String,
    pub host: String,
    pub model_code: String,
    pub protected_ports: Vec<u8>,
    pub tested_firmware: Vec<String>,
    /// The daemon never writes the lock file at `lock_path`. A lock whose text contains this
    /// value counts as its own (`change::lock_foreign_check`), so that the integration tests
    /// can hold the lock while they drive the daemon code. In service, use a value that no
    /// lock holder writes, so that every foreign lock stops a change. Empty by default.
    #[serde(default)]
    pub lock_token: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub listen: SocketAddr,
    pub agent_token: String,
    pub user_token: String,
    pub ssh_user: String,
    pub ssh_key: PathBuf,
    /// The lock file on each switch that a change checks first. `None` turns the check off.
    pub lock_path: Option<String>,
    pub data_dir: PathBuf,
    pub poll_s: u64,
    pub overlay_check_s: u64,
    pub switches: Vec<SwitchConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    listen: SocketAddr,
    agent_token: String,
    user_token: String,
    ssh_user: String,
    ssh_key: String,
    #[serde(default)]
    lock_path: Option<String>,
    data_dir: String,
    poll_s: u64,
    overlay_check_s: u64,
    switches: Vec<SwitchConfig>,
}

// The config file names paths with "~/...", as a user shell would. Nothing else in the
// crate expands "~", so this is the one place a leading "~/" turns into $HOME.
fn tilde_expand(value: &str) -> PathBuf {
    let Some(rest) = value.strip_prefix("~/") else {
        return PathBuf::from(value);
    };
    match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home).join(rest),
        Err(_) => PathBuf::from(value),
    }
}

fn mode_check(path: &Path) -> anyhow::Result<()> {
    let metadata =
        fs::metadata(path).with_context(|| format!("read metadata of {}", path.display()))?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode != CONFIG_MODE {
        bail!(
            "{} has mode {mode:03o}, must be {CONFIG_MODE:03o}",
            path.display()
        );
    }
    Ok(())
}

fn tokens_check(agent_token: &str, user_token: &str) -> anyhow::Result<()> {
    if agent_token.is_empty() || user_token.is_empty() {
        bail!("agent_token and user_token must not be empty");
    }
    if agent_token == user_token {
        bail!("agent_token and user_token must differ");
    }
    Ok(())
}

fn ssh_user_check(ssh_user: &str) -> anyhow::Result<()> {
    let valid = ssh_user
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if ssh_user.is_empty() || ssh_user.starts_with('-') || !valid {
        bail!("ssh_user {ssh_user:?} must be a user name of letters, digits, '.', '_' and '-'");
    }
    Ok(())
}

// The path goes into shell scripts on the switch without quotes, so only plain path
// characters are allowed.
pub fn lock_path_check(lock_path: &str) -> anyhow::Result<()> {
    let valid = lock_path
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c));
    if !lock_path.starts_with('/') || lock_path.ends_with('/') || !valid {
        bail!(
            "lock_path {lock_path:?} must be an absolute file path of letters, digits, '/', '.', '_' and '-'"
        );
    }
    Ok(())
}

fn switch_check(sw: &SwitchConfig, names: &mut HashSet<String>) -> anyhow::Result<()> {
    if !names.insert(sw.name.clone()) {
        bail!("switch name {:?} is used more than once", sw.name);
    }
    let model = model_from_code(&sw.model_code).ok_or_else(|| {
        anyhow::anyhow!(
            "switch {:?}: unknown model code {:?}",
            sw.name,
            sw.model_code
        )
    })?;
    let port_count = model_port_count(model);
    for &port in &sw.protected_ports {
        if port == 0 || port > port_count {
            bail!(
                "switch {:?}: protected port {port} is outside 1 to {port_count}",
                sw.name
            );
        }
    }
    Ok(())
}

fn data_dir_create(data_dir: &Path) -> anyhow::Result<()> {
    if data_dir.exists() {
        return Ok(());
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(DATA_DIR_MODE)
        .create(data_dir)
        .with_context(|| format!("create data dir {}", data_dir.display()))
}

/// Loads and validates the daemon's config file. The file must have mode 600. `agent_token`
/// and `user_token` must be non-empty and differ. `poll_s` and `overlay_check_s` must not be
/// zero, because `worker::worker_spawn` rejects a zero duration. `ssh_user` must be a plain
/// user name, and `lock_path`, if set, a plain absolute file path. Each switch needs a known
/// `model_code` and protected ports within that model's port count, and switch names must be
/// unique. `data_dir` is created with mode 700 if it does not exist yet, and is left alone if
/// it does.
pub fn config_load(path: &Path) -> anyhow::Result<Config> {
    mode_check(path)?;
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let raw: RawConfig =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;

    tokens_check(&raw.agent_token, &raw.user_token)?;
    ssh_user_check(&raw.ssh_user)?;
    if let Some(lock_path) = &raw.lock_path {
        lock_path_check(lock_path)?;
    }
    if raw.poll_s == 0 {
        bail!("poll_s must not be zero");
    }
    if raw.overlay_check_s == 0 {
        bail!("overlay_check_s must not be zero");
    }

    let mut names = HashSet::new();
    for sw in &raw.switches {
        switch_check(sw, &mut names)?;
    }

    let data_dir = tilde_expand(&raw.data_dir);
    data_dir_create(&data_dir)?;

    Ok(Config {
        listen: raw.listen,
        agent_token: raw.agent_token,
        user_token: raw.user_token,
        ssh_user: raw.ssh_user,
        ssh_key: tilde_expand(&raw.ssh_key),
        lock_path: raw.lock_path,
        data_dir,
        poll_s: raw.poll_s,
        overlay_check_s: raw.overlay_check_s,
        switches: raw.switches,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expand_leading_home() {
        // SAFETY: no other thread in this test binary reads HOME concurrently with this test.
        unsafe {
            std::env::set_var("HOME", "/home/test-user");
        }
        assert_eq!(
            tilde_expand("~/.ssh/id_rsa"),
            PathBuf::from("/home/test-user/.ssh/id_rsa")
        );
    }

    #[test]
    fn tilde_expand_leaves_other_paths() {
        assert_eq!(
            tilde_expand("/etc/uswctl.toml"),
            PathBuf::from("/etc/uswctl.toml")
        );
        assert_eq!(
            tilde_expand("relative/path"),
            PathBuf::from("relative/path")
        );
    }
}
