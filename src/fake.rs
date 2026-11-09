// Simulated switches behind `FakeTransport`, built from the saved fixtures: for the tests and `uswctld --fake`.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::change::OVERLAY_PATH;
use crate::config::{Config, SwitchConfig};
use crate::live::PROBE_HEAD;
use crate::model::{Model, model_from_code};
use crate::transport::{FakeTransport, Output};

pub const XG10_CFG: &str = include_str!("../tests/fixtures/xg10/system.cfg");
pub const XG10_RUNNING: &str = include_str!("../tests/fixtures/xg10/running-config.txt");
pub const XG10_PROBE: &str = include_str!("../tests/fixtures/xg10/probe.txt");
pub const XG10_POE: &str = include_str!("../tests/fixtures/xg10/poe-show.txt");
pub const PROMAX16_CFG: &str = include_str!("../tests/fixtures/promax16/system.cfg");
pub const PROMAX16_RUNNING: &str = include_str!("../tests/fixtures/promax16/running-config.txt");
pub const PROMAX16_MCA: &str = include_str!("../tests/fixtures/promax16/mca-dump.json");
pub const PROMAX16_POE: &str = include_str!("../tests/fixtures/promax16/poe-show.txt");

pub const FIRMWARE: &str = "7.5.15.17146";
pub const LOCK_TOKEN: &str = "ubiquity-gui";
pub const LOCK_PATH: &str = "/tmp/agent-lock";
// `change::file_read_optional` starts its script so; the test below keeps it equal to
// LOCK_PATH.
pub const LOCK_CHECK_PREFIX: &str = "if [ -e /tmp/agent-lock ]";
pub const SSH_USER: &str = "admin";
pub const NO_FILE: &str = "__USWCTL_NOFILE__\n";

pub const FAKE_LISTEN: &str = "127.0.0.1:8650";
pub const FAKE_AGENT_TOKEN: &str = "agent";
pub const FAKE_USER_TOKEN: &str = "user";
const FAKE_POLL_S: u64 = 2;
const FAKE_OVERLAY_CHECK_S: u64 = 60;
const MCA_MARKER: &str = "@@mca\n";

/// The text that one simulated switch returns for each read.
pub struct Fixtures {
    pub cfg: &'static str,
    pub running: &'static str,
    pub probe: String,
    pub poe: &'static str,
}

pub fn fixtures_xg10_get() -> Fixtures {
    Fixtures {
        cfg: XG10_CFG,
        running: XG10_RUNNING,
        probe: XG10_PROBE.to_string(),
        poe: XG10_POE,
    }
}

// Only the XG 10 has a saved probe. The Pro Max 16 takes its health sections and adds its
// own mca-dump, which holds all of its port data.
pub fn fixtures_promax16_get() -> Fixtures {
    let health_end = XG10_PROBE
        .find(MCA_MARKER)
        .map_or(XG10_PROBE.len(), |at| at + MCA_MARKER.len());
    Fixtures {
        cfg: PROMAX16_CFG,
        running: PROMAX16_RUNNING,
        probe: format!("{}{PROMAX16_MCA}", &XG10_PROBE[..health_end]),
        poe: PROMAX16_POE,
    }
}

fn fixtures_model_get(model: Model) -> Fixtures {
    match model {
        Model::Xg10 => fixtures_xg10_get(),
        Model::ProMax16 => fixtures_promax16_get(),
    }
}

/// A simulated switch. `system.cfg` never changes, so every real write ends in NoEffect at
/// the read-back; dry runs, reads and transient actions succeed.
pub struct Sim {
    pub lock: String,
    pub firmware: String,
    pub fixtures: Fixtures,
}

pub fn sim_new(fixtures: Fixtures) -> Sim {
    Sim {
        lock: String::new(),
        firmware: FIRMWARE.to_string(),
        fixtures,
    }
}

pub fn output_build(status: i32, stdout: &str) -> Output {
    Output {
        status,
        stdout: stdout.to_string(),
        stderr: String::new(),
    }
}

pub fn sim_respond(sim: &Mutex<Sim>, script: &str) -> Output {
    let sim = sim.lock().expect("sim lock");
    let fixtures = &sim.fixtures;
    if script.starts_with(PROBE_HEAD) {
        return output_build(0, &fixtures.probe);
    }
    if script.starts_with(LOCK_CHECK_PREFIX) {
        let text = if sim.lock.is_empty() {
            NO_FILE
        } else {
            &sim.lock
        };
        return output_build(0, text);
    }
    if script.contains("cfgmtd -r") || script.contains("cat /tmp/system.cfg") {
        return output_build(0, fixtures.cfg);
    }
    if script.contains("mca-dump") {
        return output_build(0, &format!("{}\n", sim.firmware));
    }
    if script.contains(&format!("cat {OVERLAY_PATH}")) {
        return output_build(0, NO_FILE);
    }
    if script.contains("show running-config") {
        return output_build(0, fixtures.running);
    }
    if script.contains("swctrl poe show") {
        return output_build(0, fixtures.poe);
    }
    if script.contains("/etc/persistent/cfg/mgmt") {
        return output_build(0, "ready\n");
    }
    if script.contains("cli -e")
        || script.starts_with("swctrl poe")
        || script.contains("apply-config")
        || script.contains("save-config")
        || script.contains("port-locate")
        || script.contains("reboot")
    {
        return output_build(0, "");
    }
    output_build(127, "")
}

/// A `FakeTransport` that answers from `sim`, and the sim itself, so that a test can flip
/// its lock or firmware.
pub fn sim_transport_build(fixtures: Fixtures) -> (Arc<FakeTransport>, Arc<Mutex<Sim>>) {
    let sim = Arc::new(Mutex::new(sim_new(fixtures)));
    let sim_ref = sim.clone();
    let fake = Arc::new(FakeTransport::fake_new(move |script| {
        sim_respond(&sim_ref, script)
    }));
    (fake, sim)
}

// The hosts are from the documentation range (RFC 5737), so that no fake switch can reach
// a real device.
pub fn switch_config_xg10_build() -> SwitchConfig {
    SwitchConfig {
        name: "xg10".to_string(),
        host: "192.0.2.10".to_string(),
        model_code: "USWED77".to_string(),
        protected_ports: vec![11],
        tested_firmware: vec![FIRMWARE.to_string()],
        lock_token: LOCK_TOKEN.to_string(),
    }
}

pub fn switch_config_promax16_build() -> SwitchConfig {
    SwitchConfig {
        name: "promax16".to_string(),
        host: "192.0.2.11".to_string(),
        model_code: "USPM16P".to_string(),
        protected_ports: vec![14, 17],
        tested_firmware: vec![FIRMWARE.to_string()],
        lock_token: LOCK_TOKEN.to_string(),
    }
}

/// The config of `uswctld --fake`: both switch models, simulated, with fixed tokens.
pub fn fake_config_build(data_dir: &Path) -> Config {
    let listen: SocketAddr = FAKE_LISTEN
        .parse()
        .expect("FAKE_LISTEN is a socket address");
    Config {
        listen,
        agent_token: FAKE_AGENT_TOKEN.to_string(),
        user_token: FAKE_USER_TOKEN.to_string(),
        ssh_user: SSH_USER.to_string(),
        ssh_key: data_dir.join("id_rsa"),
        lock_path: Some(LOCK_PATH.to_string()),
        data_dir: data_dir.to_path_buf(),
        poll_s: FAKE_POLL_S,
        overlay_check_s: FAKE_OVERLAY_CHECK_S,
        switches: vec![switch_config_promax16_build(), switch_config_xg10_build()],
    }
}

/// The fixtures that fit the model of `sw`. `None` for an unknown model code.
pub fn fixtures_switch_get(sw: &SwitchConfig) -> Option<Fixtures> {
    model_from_code(&sw.model_code).map(fixtures_model_get)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_check_prefix_names_lock_path() {
        assert_eq!(LOCK_CHECK_PREFIX, format!("if [ -e {LOCK_PATH} ]"));
    }
}
