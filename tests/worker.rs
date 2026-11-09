// Integration tests for the device worker: request order, polling, actions and overlay repair.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use uswctl::backup::BackupStore;
use uswctl::change::{Change, ChangeError, SwitchCtx};
use uswctl::live::{PROBE_HEAD, probe_script_build};
use uswctl::locate::LOCATE_OFF_SCRIPT;
use uswctl::model::Model;
use uswctl::overlay::{Overlay, overlay_render};
use uswctl::settings::{BpduAction, PortSettingsPatch};
use uswctl::transport::{FakeTransport, Output, Transport};
use uswctl::worker::{
    Action, DeviceHandle, DeviceState, EventKind, action_script_build, worker_spawn,
};

const XG10_CFG: &str = include_str!("fixtures/xg10/system.cfg");
const XG10_RUNNING: &str = include_str!("fixtures/xg10/running-config.txt");
const XG10_PROBE: &str = include_str!("fixtures/xg10/probe.txt");

const TOKEN: &str = "ubiquity-gui";
const OVERLAY_PATH: &str = "/etc/persistent/uswctl-overlay.scr";
const OVERLAY_RUN: &str = "cli -e /etc/persistent/uswctl-overlay.scr";
const ACTION_PATH: &str = "/tmp/uswctl-action.scr";
const LOCK_PATH: &str = "/tmp/agent-lock";
const LOCK_CHECK_PREFIX: &str = "if [ -e /tmp/agent-lock ]";
const NO_FILE: &str = "__USWCTL_NOFILE__\n";
const UPTIME_FIXTURE: &str = "6636.82 5792.36";
const MD5_FIXTURE: &str = "54361984c7c836fface2416b626fef2c";
const INFORM_URL: &str = "http://192.0.2.114:8080/inform";
const POLL: Duration = Duration::from_secs(5);
const HOUR: Duration = Duration::from_secs(3600);

fn ctx_xg10() -> SwitchCtx {
    SwitchCtx {
        name: "xg10".to_string(),
        model: Model::Xg10,
        lock_path: Some(LOCK_PATH.to_string()),
        lock_token: TOKEN.to_string(),
        tested_firmware: vec!["7.5.15.17146".to_string()],
    }
}

fn out(status: i32, stdout: &str) -> Output {
    Output {
        status,
        stdout: stdout.to_string(),
        stderr: String::new(),
    }
}

// The state of the simulated switch. A test changes it while the worker runs.
// `overlay_active` tells whether the running config shows the global BPDU filtering line;
// `cli -e` of the overlay file sets it, a reboot clears it.
struct Sim {
    probe: Option<String>,
    syscfg: String,
    overlay: Option<String>,
    overlay_active: bool,
    lock: String,
    action_status: i32,
    action_stderr: String,
    locate_status: i32,
    // Every script except the probe fails as an SSH connection failure.
    scripts_down: bool,
}

impl Sim {
    fn xg10() -> Sim {
        Sim {
            probe: Some(XG10_PROBE.to_string()),
            syscfg: XG10_CFG.to_string(),
            overlay: None,
            overlay_active: false,
            lock: String::new(),
            action_status: 0,
            action_stderr: String::new(),
            locate_status: 0,
            scripts_down: false,
        }
    }

    fn running_get(&self) -> String {
        if self.overlay_active {
            format!("{XG10_RUNNING}spanning-tree bpdu filtering\n")
        } else {
            XG10_RUNNING.to_string()
        }
    }
}

fn overlay_filtering() -> String {
    let overlay = Overlay {
        bpdu_action: BpduAction::Filtering,
        ..Overlay::default()
    };
    overlay_render(&overlay, Model::Xg10)
}

fn sim_respond(sim: &Mutex<Sim>, script: &str) -> Output {
    let mut sim = sim.lock().unwrap();
    if script.starts_with(PROBE_HEAD) {
        return match &sim.probe {
            Some(text) => out(0, text),
            None => Output {
                status: 255,
                stdout: String::new(),
                stderr: "connection refused".to_string(),
            },
        };
    }
    if sim.scripts_down {
        return Output {
            status: 255,
            stdout: String::new(),
            stderr: "connection timed out".to_string(),
        };
    }
    if script.starts_with(LOCK_CHECK_PREFIX) {
        let text = if sim.lock.is_empty() {
            NO_FILE.to_string()
        } else {
            sim.lock.clone()
        };
        return out(0, &text);
    }
    if script.contains("cfgmtd -r") {
        return out(0, &sim.syscfg);
    }
    if script.contains("mca-dump") {
        return out(0, "7.5.15.17146\n");
    }
    if script.contains("cat /tmp/system.cfg") {
        return out(0, &sim.syscfg);
    }
    if script.contains(&format!("cat {OVERLAY_PATH}")) {
        let text = sim.overlay.clone().unwrap_or_else(|| NO_FILE.to_string());
        return out(0, &text);
    }
    if script.contains("show running-config") {
        return out(0, &sim.running_get());
    }
    if script == OVERLAY_RUN {
        sim.overlay_active = true;
        return out(0, "");
    }
    if script.contains(&format!("cli -e {ACTION_PATH}"))
        || script.starts_with("swctrl poe set")
        || script.starts_with("swctrl poe restart")
    {
        return Output {
            status: sim.action_status,
            stdout: String::new(),
            stderr: sim.action_stderr.clone(),
        };
    }
    if script.contains("/etc/persistent/cfg/mgmt") {
        return out(0, "ready\n");
    }
    if script.contains("port-locate") {
        return Output {
            status: sim.locate_status,
            stdout: String::new(),
            stderr: if sim.locate_status == 0 {
                String::new()
            } else {
                "led driver busy".to_string()
            },
        };
    }
    if script.contains("apply-config") || script.contains("save-config") {
        return out(0, "");
    }
    if script.contains("reboot") {
        return out(0, "");
    }
    out(127, "")
}

fn fake_new(sim: Arc<Mutex<Sim>>) -> Arc<FakeTransport> {
    Arc::new(FakeTransport::fake_new(move |script| {
        sim_respond(&sim, script)
    }))
}

// Every call waits, so that two requests that run at the same time would interleave their
// scripts in the fake's record.
struct SlowTransport {
    inner: Arc<FakeTransport>,
}

#[async_trait]
impl Transport for SlowTransport {
    async fn script_run(&self, script: &str) -> anyhow::Result<Output> {
        tokio::time::sleep(Duration::from_millis(10)).await;
        self.inner.script_run(script).await
    }

    async fn file_put(&self, path: &str, data: &[u8]) -> anyhow::Result<()> {
        tokio::time::sleep(Duration::from_millis(10)).await;
        self.inner.file_put(path, data).await
    }
}

fn store_temp() -> (tempfile::TempDir, BackupStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = BackupStore {
        dir: dir.path().join("backups"),
    };
    (dir, store)
}

fn spawn(
    fake: &Arc<FakeTransport>,
    ctx: SwitchCtx,
    store: BackupStore,
    poll: Duration,
    overlay_check: Duration,
) -> DeviceHandle {
    let t: Arc<dyn Transport> = fake.clone();
    worker_spawn(t, ctx, vec![11], store, poll, overlay_check).unwrap()
}

fn events_count(state: &DeviceState, kind: &EventKind) -> usize {
    state.events.iter().filter(|e| &e.kind == kind).count()
}

fn scripts_count(fake: &FakeTransport, needle: &str) -> usize {
    fake.scripts_get()
        .iter()
        .filter(|s| s.contains(needle))
        .count()
}

fn probe_set(sim: &Mutex<Sim>, from: &str, to: &str) {
    let mut sim = sim.lock().unwrap();
    let text = sim.probe.as_ref().unwrap().replace(from, to);
    sim.probe = Some(text);
}

fn name_change(idx: u8, name: &str) -> Change {
    Change::Port {
        idx,
        patch: PortSettingsPatch {
            name: Some(name.to_string()),
            ..Default::default()
        },
    }
}

#[tokio::test(start_paused = true)]
async fn requests_are_serialized() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let t: Arc<dyn Transport> = Arc::new(SlowTransport {
        inner: fake.clone(),
    });
    let handle = worker_spawn(t, ctx_xg10(), vec![11], store, HOUR, HOUR).unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;

    let (a, b) = tokio::join!(
        handle.change_run(name_change(3, "DUT A"), false),
        handle.change_run(name_change(5, "DUT B"), false),
    );
    // The fake shows the old system.cfg after the write, so both changes fail the
    // read-back. The order of the scripts is the point here, not the result.
    assert!(a.is_err() && b.is_err(), "{a:?} {b:?}");

    let scripts = fake.scripts_get();
    let locks: Vec<usize> = scripts
        .iter()
        .enumerate()
        .filter(|(_, s)| s.starts_with(LOCK_CHECK_PREFIX))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(locks.len(), 2, "{scripts:#?}");
    let first = &scripts[locks[0]..locks[1]];
    let second = &scripts[locks[1]..];
    for part in [first, second] {
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

#[tokio::test(start_paused = true)]
async fn reboot_clears_transient_and_repairs_overlay() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    sim.lock().unwrap().overlay = Some(overlay_filtering());
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    tokio::time::sleep(Duration::from_secs(1)).await;

    // The overlay check at start finds the overlay missing and runs it once.
    assert_eq!(scripts_count(&fake, OVERLAY_RUN), 1);
    handle.action_run(1, Action::LinkDown).await.unwrap();
    handle.action_run(2, Action::PoeOff).await.unwrap();
    let state = handle.state_get().await;
    assert_eq!(state.transient.get(&1), Some(&vec![Action::LinkDown]));
    assert_eq!(state.transient.get(&2), Some(&vec![Action::PoeOff]));
    assert!(state.overlay_last_run.is_some());

    {
        let mut sim = sim.lock().unwrap();
        sim.overlay_active = false;
    }
    probe_set(&sim, UPTIME_FIXTURE, "50.00 40.00");
    tokio::time::sleep(POLL).await;

    let state = handle.state_get().await;
    assert_eq!(events_count(&state, &EventKind::Reboot), 1);
    let reboot = state
        .events
        .iter()
        .find(|e| e.kind == EventKind::Reboot)
        .unwrap();
    assert!(!reboot.requested);
    assert!(state.transient.is_empty(), "{:?}", state.transient);
    assert_eq!(scripts_count(&fake, OVERLAY_RUN), 2);
    assert_eq!(events_count(&state, &EventKind::OverlayRepaired), 2);
    assert_eq!(state.uptime_s, 50.0);
}

#[tokio::test(start_paused = true)]
async fn requested_reboot_is_marked() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    tokio::time::sleep(Duration::from_secs(1)).await;

    handle.reboot_run().await.unwrap();
    assert_eq!(scripts_count(&fake, "command -v syswrapper.sh"), 1);
    probe_set(&sim, UPTIME_FIXTURE, "50.00 40.00");
    tokio::time::sleep(POLL).await;

    let state = handle.state_get().await;
    let reboots: Vec<_> = state
        .events
        .iter()
        .filter(|e| e.kind == EventKind::Reboot)
        .collect();
    assert_eq!(reboots.len(), 1);
    assert!(reboots[0].requested);

    // The mark holds for one uptime drop only.
    probe_set(&sim, "50.00 40.00", "20.00 10.00");
    tokio::time::sleep(POLL).await;
    let state = handle.state_get().await;
    let last = state
        .events
        .iter()
        .rfind(|e| e.kind == EventKind::Reboot)
        .unwrap();
    assert!(!last.requested);
}

#[tokio::test(start_paused = true)]
async fn reboot_refused_by_lock() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    sim.lock().unwrap().lock = "other-session host-b 2026-11-03T10:00:00Z x\n".into();
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    let result = handle.reboot_run().await;
    assert!(matches!(result, Err(ChangeError::Locked(_))), "{result:?}");
    assert_eq!(scripts_count(&fake, "reboot"), 0);
}

#[tokio::test(start_paused = true)]
async fn external_cfg_change_detected() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    tokio::time::sleep(Duration::from_secs(1)).await;

    let state = handle.state_get().await;
    assert_eq!(events_count(&state, &EventKind::CfgChangedExternally), 0);
    let port3 = state.ports.iter().find(|p| p.idx == 3).unwrap();
    assert_ne!(port3.settings.name, "Changed 3");

    {
        let mut sim = sim.lock().unwrap();
        let mut cfg = uswctl::syscfg::syscfg_parse(&sim.syscfg);
        uswctl::syscfg::syscfg_set(&mut cfg, "switch.port.3.name", "Changed 3");
        sim.syscfg = uswctl::syscfg::syscfg_render(&cfg);
    }
    probe_set(&sim, MD5_FIXTURE, "00000000000000000000000000000001");
    tokio::time::sleep(POLL).await;

    let state = handle.state_get().await;
    assert_eq!(events_count(&state, &EventKind::CfgChangedExternally), 1);
    let port3 = state.ports.iter().find(|p| p.idx == 3).unwrap();
    assert_eq!(port3.settings.name, "Changed 3");

    tokio::time::sleep(POLL).await;
    let state = handle.state_get().await;
    assert_eq!(events_count(&state, &EventKind::CfgChangedExternally), 1);
}

#[tokio::test(start_paused = true)]
async fn own_change_is_not_external() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    tokio::time::sleep(Duration::from_secs(1)).await;

    let _ = handle.change_run(name_change(3, "DUT 3"), false).await;
    probe_set(&sim, MD5_FIXTURE, "00000000000000000000000000000001");
    tokio::time::sleep(POLL).await;
    let state = handle.state_get().await;
    assert_eq!(events_count(&state, &EventKind::CfgChangedExternally), 0);
    assert!(state.last_failure.is_some());
}

#[tokio::test(start_paused = true)]
async fn refused_change_keeps_external_watch() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    tokio::time::sleep(Duration::from_secs(1)).await;

    sim.lock().unwrap().lock = "other-session host-b 2026-11-03T10:00:00Z x\n".into();
    let result = handle.change_run(name_change(3, "DUT 3"), false).await;
    assert!(matches!(result, Err(ChangeError::Locked(_))), "{result:?}");
    probe_set(&sim, MD5_FIXTURE, "00000000000000000000000000000001");
    tokio::time::sleep(POLL).await;
    let state = handle.state_get().await;
    assert_eq!(events_count(&state, &EventKind::CfgChangedExternally), 1);
}

#[tokio::test(start_paused = true)]
async fn dry_run_writes_nothing() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    let result = handle
        .change_run(name_change(3, "DUT 3"), true)
        .await
        .unwrap();
    assert_eq!(result.keys.len(), 1);
    assert!(fake.file_puts_get().is_empty());
}

#[tokio::test(start_paused = true)]
async fn inform_connected_event() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    tokio::time::sleep(Duration::from_secs(1)).await;

    let connected = EventKind::InformConnected(INFORM_URL.to_string());
    let state = handle.state_get().await;
    assert_eq!(events_count(&state, &connected), 1, "{:?}", state.events);

    tokio::time::sleep(POLL).await;
    let state = handle.state_get().await;
    assert_eq!(events_count(&state, &connected), 1);
}

#[tokio::test(start_paused = true)]
async fn offline_log_added_event() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let state = handle.state_get().await;
    assert!(
        !state
            .events
            .iter()
            .any(|e| matches!(e.kind, EventKind::OfflineLogAdded(_)))
    );

    probe_set(
        &sim,
        "timestamp_1793641000.txt\n",
        "timestamp_1793641000.txt\noffline_latest_1793650000.tar.gz\n",
    );
    tokio::time::sleep(POLL).await;
    let state = handle.state_get().await;
    let added = EventKind::OfflineLogAdded("offline_latest_1793650000.tar.gz".to_string());
    assert_eq!(events_count(&state, &added), 1, "{:?}", state.events);
}

#[tokio::test(start_paused = true)]
async fn events_keep_the_last_200() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    tokio::time::sleep(Duration::from_secs(1)).await;

    let files: String = (0..250).map(|i| format!("extra_{i:03}.tar.gz\n")).collect();
    probe_set(
        &sim,
        "timestamp_1793641000.txt\n",
        &format!("timestamp_1793641000.txt\n{files}"),
    );
    tokio::time::sleep(POLL).await;
    let state = handle.state_get().await;
    assert_eq!(state.events.len(), 200);
    assert_eq!(
        state.events.last().unwrap().kind,
        EventKind::OfflineLogAdded("extra_249.tar.gz".to_string())
    );
}

#[tokio::test(start_paused = true)]
async fn mem_sample_every_ten_minutes() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    tokio::time::sleep(Duration::from_secs(25 * 60 + 2)).await;
    let state = handle.state_get().await;
    assert_eq!(state.mem_series.len(), 3, "{:?}", state.mem_series);
    assert!(state.mem_series.iter().all(|(_, kb)| *kb == 423_800));
}

#[tokio::test(start_paused = true)]
async fn probe_error_then_recovery() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    sim.lock().unwrap().probe = None;
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    tokio::time::sleep(Duration::from_secs(1) + POLL * 2).await;

    let state = handle.state_get().await;
    assert_eq!(events_count(&state, &EventKind::Unreachable), 1);
    assert!(state.ports.is_empty());

    sim.lock().unwrap().probe = Some(XG10_PROBE.to_string());
    tokio::time::sleep(POLL).await;
    let state = handle.state_get().await;
    assert_eq!(events_count(&state, &EventKind::Unreachable), 1);
    assert_eq!(state.uptime_s, 6636.82);
    assert_eq!(state.ports.len(), 12);
    assert_eq!(state.firmware, "7.5.15.17146");
    assert!(state.firmware_tested);
    let port11 = state.ports.iter().find(|p| p.idx == 11).unwrap();
    assert!(port11.protected);
    assert_eq!(port11.cli_name, "te11");

    sim.lock().unwrap().probe = None;
    tokio::time::sleep(POLL).await;
    let state = handle.state_get().await;
    assert_eq!(events_count(&state, &EventKind::Unreachable), 2);
}

#[test]
fn link_down_script_uses_cli_name() {
    let script = action_script_build(Model::Xg10, 1, Action::LinkDown);
    assert!(script.contains("interface te1\n"), "{script}");
    assert!(script.contains("\n shutdown\n"), "{script}");
    assert!(script.contains("exit"), "{script}");
    let up = action_script_build(Model::ProMax16, 17, Action::LinkUp);
    assert!(up.contains("interface te1\n"), "{up}");
    assert!(up.contains("\n no shutdown\n"), "{up}");
    assert_eq!(
        action_script_build(Model::Xg10, 1, Action::PoeOff),
        "swctrl poe set off id 1"
    );
    assert_eq!(
        action_script_build(Model::Xg10, 1, Action::PoeCycle),
        "swctrl poe restart id 1"
    );
}

#[tokio::test(start_paused = true)]
async fn link_action_puts_file_and_runs_cli() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    let script = handle.action_run(1, Action::LinkDown).await.unwrap();
    assert!(script.contains("interface te1"), "{script}");

    let files = fake.files_get();
    let text = String::from_utf8(files.get(ACTION_PATH).unwrap().clone()).unwrap();
    assert!(text.contains("interface te1"));
    let scripts = fake.scripts_get();
    let run = scripts
        .iter()
        .find(|s| s.contains(&format!("cli -e {ACTION_PATH}")))
        .unwrap();
    assert!(run.contains(&format!("rm -f {ACTION_PATH}")), "{run}");

    handle.action_run(1, Action::LinkDown).await.unwrap();
    let state = handle.state_get().await;
    assert_eq!(state.transient.get(&1), Some(&vec![Action::LinkDown]));
    handle.action_run(1, Action::LinkUp).await.unwrap();
    let state = handle.state_get().await;
    assert!(state.transient.is_empty());
}

#[tokio::test(start_paused = true)]
async fn action_failure_reports_stderr() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    {
        let mut sim = sim.lock().unwrap();
        sim.action_status = 1;
        sim.action_stderr = "port has no PoE".to_string();
    }
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    let result = handle.action_run(12, Action::PoeOff).await;
    match result {
        Err(ChangeError::ApplyFailed(text)) => assert!(text.contains("port has no PoE")),
        other => panic!("expected ApplyFailed, got {other:?}"),
    }
    let state = handle.state_get().await;
    assert!(state.transient.is_empty());
    assert!(state.last_failure.unwrap().contains("port has no PoE"));
}

#[tokio::test(start_paused = true)]
async fn action_refused_by_lock_and_range() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    sim.lock().unwrap().lock = "other-session host-b 2026-11-03T10:00:00Z x\n".into();
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    let result = handle.action_run(1, Action::PoeOff).await;
    assert!(matches!(result, Err(ChangeError::Locked(_))), "{result:?}");
    let result = handle.action_run(13, Action::PoeOff).await;
    assert!(
        matches!(result, Err(ChangeError::Settings(_))),
        "{result:?}"
    );
    assert_eq!(scripts_count(&fake, "swctrl poe set"), 0);
    assert!(fake.file_puts_get().is_empty());
}

#[tokio::test(start_paused = true)]
async fn untested_firmware_allows_actions_and_repair() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    sim.lock().unwrap().overlay = Some(overlay_filtering());
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let ctx = SwitchCtx {
        tested_firmware: vec!["7.6.0.1".to_string()],
        ..ctx_xg10()
    };
    let handle = spawn(&fake, ctx, store, POLL, HOUR);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(scripts_count(&fake, OVERLAY_RUN), 1);
    handle.action_run(1, Action::PoeCycle).await.unwrap();
    let state = handle.state_get().await;
    assert!(!state.firmware_tested);
}

#[tokio::test(start_paused = true)]
async fn overlay_check_without_file_or_when_present() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, Duration::from_secs(30));
    tokio::time::sleep(Duration::from_secs(61)).await;
    assert_eq!(scripts_count(&fake, OVERLAY_RUN), 0);

    {
        let mut sim = sim.lock().unwrap();
        sim.overlay = Some(overlay_filtering());
        sim.overlay_active = true;
    }
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(scripts_count(&fake, OVERLAY_RUN), 0);

    sim.lock().unwrap().overlay_active = false;
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(scripts_count(&fake, OVERLAY_RUN), 1);
    let state = handle.state_get().await;
    assert_eq!(events_count(&state, &EventKind::OverlayRepaired), 1);
}

#[tokio::test(start_paused = true)]
async fn overlay_repair_waits_for_foreign_lock() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    sim.lock().unwrap().overlay = Some(overlay_filtering());
    probe_set(
        &sim,
        "@@lock\n",
        "@@lock\nother-session host-b 2026-11-03T10:00:00Z firmware\n",
    );
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, Duration::from_secs(30));
    tokio::time::sleep(Duration::from_secs(61)).await;
    assert_eq!(scripts_count(&fake, OVERLAY_RUN), 0);
    let state = handle.state_get().await;
    assert_eq!(events_count(&state, &EventKind::OverlayRepaired), 0);

    probe_set(
        &sim,
        "@@lock\nother-session host-b 2026-11-03T10:00:00Z firmware\n",
        "@@lock\n",
    );
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(scripts_count(&fake, OVERLAY_RUN), 1);
}

// Without a parsed probe the worker has no lock text of its own, so it reads the file.
#[tokio::test(start_paused = true)]
async fn overlay_repair_reads_lock_without_probe() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    {
        let mut sim = sim.lock().unwrap();
        sim.overlay = Some(overlay_filtering());
        sim.probe = None;
        sim.lock = "other-session host-b 2026-11-03T10:00:00Z firmware\n".to_string();
    }
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, Duration::from_secs(30));
    tokio::time::sleep(Duration::from_secs(61)).await;
    assert!(scripts_count(&fake, LOCK_CHECK_PREFIX) >= 2);
    assert_eq!(scripts_count(&fake, OVERLAY_RUN), 0);
    assert_eq!(
        events_count(&handle.state_get().await, &EventKind::OverlayRepaired),
        0
    );

    sim.lock().unwrap().lock = String::new();
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(scripts_count(&fake, OVERLAY_RUN), 1);
    assert_eq!(
        events_count(&handle.state_get().await, &EventKind::OverlayRepaired),
        1
    );
}

// Without a lock path the worker reads no lock file, so a foreign lock does not stop the
// repair.
#[tokio::test(start_paused = true)]
async fn overlay_repair_without_lock_path() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    {
        let mut sim = sim.lock().unwrap();
        sim.overlay = Some(overlay_filtering());
        sim.probe = None;
        sim.lock = "other-session host-b 2026-11-03T10:00:00Z firmware\n".to_string();
    }
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let ctx = SwitchCtx {
        lock_path: None,
        ..ctx_xg10()
    };
    let handle = spawn(&fake, ctx, store, POLL, Duration::from_secs(30));
    tokio::time::sleep(Duration::from_secs(31)).await;
    assert_eq!(scripts_count(&fake, LOCK_CHECK_PREFIX), 0);
    assert_eq!(scripts_count(&fake, OVERLAY_RUN), 1);
    assert_eq!(
        events_count(&handle.state_get().await, &EventKind::OverlayRepaired),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn overlay_repair_runs_under_own_lock() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    sim.lock().unwrap().overlay = Some(overlay_filtering());
    probe_set(
        &sim,
        "@@lock\n",
        &format!("@@lock\n{TOKEN} host-b 2026-11-03T10:00:00Z test\n"),
    );
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, Duration::from_secs(30));
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(scripts_count(&fake, OVERLAY_RUN), 1);
    let state = handle.state_get().await;
    assert_eq!(events_count(&state, &EventKind::OverlayRepaired), 1);
}

#[tokio::test]
async fn zero_durations_rejected() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim);
    let (_dir, store) = store_temp();
    let t: Arc<dyn Transport> = fake.clone();
    let zero_poll = worker_spawn(
        t.clone(),
        ctx_xg10(),
        vec![],
        store.clone(),
        Duration::ZERO,
        HOUR,
    );
    assert!(zero_poll.is_err());
    let zero_check = worker_spawn(t, ctx_xg10(), vec![], store, POLL, Duration::ZERO);
    assert!(zero_check.is_err());
    assert!(fake.scripts_get().is_empty());
}

// The overlay check and the poll run on their own timers. When the switch answers again
// between a poll and an overlay check, the overlay check repairs the overlay before the
// poll sees the uptime drop, and the repair after the Reboot event finds nothing to do.
#[tokio::test(start_paused = true)]
async fn overlay_check_before_reboot_poll_repairs_once() {
    let poll = Duration::from_secs(10);
    let overlay_check = Duration::from_secs(15);
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    {
        let mut sim = sim.lock().unwrap();
        sim.overlay = Some(overlay_filtering());
        sim.overlay_active = true;
    }
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, poll, overlay_check);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(scripts_count(&fake, OVERLAY_RUN), 0);

    // The switch reboots at 1 s and answers again at 12 s, after the poll at 10 s and
    // before the overlay check at 15 s.
    let probe_up = sim.lock().unwrap().probe.take();
    {
        let mut sim = sim.lock().unwrap();
        sim.overlay_active = false;
    }
    tokio::time::sleep(Duration::from_secs(11)).await;
    sim.lock().unwrap().probe = probe_up;
    probe_set(&sim, UPTIME_FIXTURE, "50.00 40.00");
    tokio::time::sleep(Duration::from_secs(10)).await;

    let state = handle.state_get().await;
    let position = |kind: &EventKind| state.events.iter().position(|e| &e.kind == kind);
    let reboot = position(&EventKind::Reboot).expect("Reboot event");
    let repaired = position(&EventKind::OverlayRepaired).expect("OverlayRepaired event");
    assert!(repaired < reboot, "{:?}", state.events);
    assert_eq!(events_count(&state, &EventKind::OverlayRepaired), 1);
    assert_eq!(scripts_count(&fake, OVERLAY_RUN), 1);
}

fn master_resets_count(fake: &FakeTransport) -> usize {
    fake.scripts_get()
        .iter()
        .filter(|s| *s == "#master_reset")
        .count()
}

#[tokio::test(start_paused = true)]
async fn master_reset_after_probe_connection_error() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let _handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(master_resets_count(&fake), 0);

    sim.lock().unwrap().probe = None;
    tokio::time::sleep(POLL).await;
    let scripts = fake.scripts_get();
    let last_probe = scripts
        .iter()
        .rposition(|s| *s == probe_script_build(Some(LOCK_PATH)))
        .unwrap();
    assert_eq!(master_resets_count(&fake), 1);
    assert_eq!(scripts[last_probe + 1], "#master_reset");
}

#[tokio::test(start_paused = true)]
async fn master_reset_after_reboot_before_repair() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    {
        let mut sim = sim.lock().unwrap();
        sim.overlay = Some(overlay_filtering());
        sim.overlay_active = true;
    }
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(master_resets_count(&fake), 0);

    sim.lock().unwrap().overlay_active = false;
    probe_set(&sim, UPTIME_FIXTURE, "50.00 40.00");
    tokio::time::sleep(POLL).await;

    let state = handle.state_get().await;
    assert_eq!(events_count(&state, &EventKind::Reboot), 1);
    assert_eq!(events_count(&state, &EventKind::OverlayRepaired), 1);
    let scripts = fake.scripts_get();
    let reset = scripts.iter().position(|s| s == "#master_reset").unwrap();
    let repair = scripts.iter().position(|s| s == OVERLAY_RUN).unwrap();
    assert_eq!(master_resets_count(&fake), 1);
    assert!(reset < repair, "{scripts:?}");
}

#[tokio::test(start_paused = true)]
async fn master_reset_after_request_connection_error() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    tokio::time::sleep(Duration::from_secs(1)).await;

    sim.lock().unwrap().scripts_down = true;
    let result = handle.action_run(1, Action::LinkDown).await;
    assert!(
        matches!(result, Err(ChangeError::Unreachable(_))),
        "{result:?}"
    );
    assert_eq!(master_resets_count(&fake), 1);

    sim.lock().unwrap().scripts_down = false;
    sim.lock().unwrap().lock = "other-session host-b".to_string();
    let result = handle.action_run(1, Action::LinkDown).await;
    assert!(matches!(result, Err(ChangeError::Locked(_))), "{result:?}");
    assert_eq!(master_resets_count(&fake), 1);
}

// mca-dump gives JSON without a port table for a while after a boot.
#[tokio::test(start_paused = true)]
async fn probe_parse_failure_keeps_state() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(handle.state_get().await.ports.len(), 12);

    let good = sim.lock().unwrap().probe.clone().unwrap();
    let mca_at = good.find("@@mca\n").unwrap();
    sim.lock().unwrap().probe = Some(format!(
        "{}@@mca\n{{\"version\": \"7.5.15.17146\"}}\n",
        &good[..mca_at]
    ));
    tokio::time::sleep(POLL * 2).await;

    let state = handle.state_get().await;
    assert_eq!(events_count(&state, &EventKind::Unreachable), 0);
    assert_eq!(state.ports.len(), 12);
    assert!(
        state
            .last_failure
            .as_deref()
            .is_some_and(|f| f.contains("port_table")),
        "{:?}",
        state.last_failure
    );
    assert_eq!(master_resets_count(&fake), 0);
}

const FOREIGN_LOCK: &str = "other-session host-b 2026-11-03T10:00:00Z x\n";
const LOCATE_SET: &str = "syswrapper.sh set-port-locate";

#[tokio::test(start_paused = true)]
async fn locate_sets_state_and_ends_at_deadline() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    tokio::time::sleep(Duration::from_secs(1)).await;

    let before = std::time::SystemTime::now();
    let (locate, script) = handle
        .locate_run(vec![2, 1], Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(script, "syswrapper.sh set-port-locate 0 3");
    assert_eq!(locate.ports, vec![1, 2]);
    assert!(
        locate.until >= before + Duration::from_secs(59),
        "{locate:?}"
    );
    let state = handle.state_get().await;
    assert_eq!(
        state.locate.as_ref().map(|l| l.ports.clone()),
        Some(vec![1, 2])
    );
    assert_eq!(scripts_count(&fake, LOCATE_SET), 1);
    assert_eq!(scripts_count(&fake, LOCATE_OFF_SCRIPT), 0);

    tokio::time::sleep(Duration::from_secs(55)).await;
    assert_eq!(scripts_count(&fake, LOCATE_OFF_SCRIPT), 0);
    assert!(handle.state_get().await.locate.is_some());
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(scripts_count(&fake, LOCATE_OFF_SCRIPT), 1);
    assert!(handle.state_get().await.locate.is_none());

    tokio::time::sleep(Duration::from_secs(120)).await;
    assert_eq!(scripts_count(&fake, LOCATE_OFF_SCRIPT), 1);
}

#[tokio::test(start_paused = true)]
async fn locate_stop_ends_at_once() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    handle
        .locate_run(vec![5], Duration::from_secs(300))
        .await
        .unwrap();
    let script = handle.locate_off_run().await.unwrap();
    assert_eq!(script, LOCATE_OFF_SCRIPT);
    assert_eq!(scripts_count(&fake, LOCATE_OFF_SCRIPT), 1);
    assert!(handle.state_get().await.locate.is_none());

    // The ended locate has no deadline left.
    tokio::time::sleep(Duration::from_secs(400)).await;
    assert_eq!(scripts_count(&fake, LOCATE_OFF_SCRIPT), 1);
}

#[tokio::test(start_paused = true)]
async fn locate_replaces_running_locate() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    handle
        .locate_run(vec![1], Duration::from_secs(30))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(10)).await;
    let (locate, script) = handle
        .locate_run(vec![12], Duration::from_secs(100))
        .await
        .unwrap();
    assert_eq!(script, "syswrapper.sh set-port-locate 0 800");
    assert_eq!(locate.ports, vec![12]);

    // The first deadline (30 s) passes without an end; the second one ends the locate.
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(scripts_count(&fake, LOCATE_OFF_SCRIPT), 0);
    let state = handle.state_get().await;
    assert_eq!(state.locate.map(|l| l.ports), Some(vec![12]));
    tokio::time::sleep(Duration::from_secs(45)).await;
    assert_eq!(scripts_count(&fake, LOCATE_OFF_SCRIPT), 1);
    assert!(handle.state_get().await.locate.is_none());
}

#[tokio::test(start_paused = true)]
async fn locate_cleared_by_reboot() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    tokio::time::sleep(Duration::from_secs(1)).await;
    handle
        .locate_run(vec![3], Duration::from_secs(300))
        .await
        .unwrap();
    probe_set(&sim, UPTIME_FIXTURE, "50.00 40.00");
    tokio::time::sleep(POLL).await;
    let state = handle.state_get().await;
    assert_eq!(events_count(&state, &EventKind::Reboot), 1);
    assert!(state.locate.is_none());

    // A reboot ends the locate on the switch, so no unset follows at the old deadline.
    tokio::time::sleep(Duration::from_secs(400)).await;
    assert_eq!(scripts_count(&fake, LOCATE_OFF_SCRIPT), 0);
}

// A new locate respects a foreign lock; a stop only gives the LEDs back, so it runs anyway.
#[tokio::test(start_paused = true)]
async fn locate_refused_by_foreign_lock_stop_allowed() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    handle
        .locate_run(vec![2], Duration::from_secs(60))
        .await
        .unwrap();
    sim.lock().unwrap().lock = FOREIGN_LOCK.into();
    let result = handle.locate_run(vec![1], Duration::from_secs(60)).await;
    assert!(matches!(result, Err(ChangeError::Locked(_))), "{result:?}");
    assert_eq!(scripts_count(&fake, LOCATE_SET), 1);
    let state = handle.state_get().await;
    assert_eq!(state.locate.map(|l| l.ports), Some(vec![2]));

    let script = handle.locate_off_run().await.unwrap();
    assert_eq!(script, LOCATE_OFF_SCRIPT);
    assert_eq!(scripts_count(&fake, LOCATE_OFF_SCRIPT), 1);
    assert!(handle.state_get().await.locate.is_none());
}

// A set that did not reach the switch for sure can still have started the locate, so the
// worker keeps the locate and ends it at its deadline.
#[tokio::test(start_paused = true)]
async fn locate_unreachable_set_still_ends() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    sim.lock().unwrap().locate_status = 255;
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    tokio::time::sleep(Duration::from_secs(1)).await;
    let result = handle.locate_run(vec![4], Duration::from_secs(30)).await;
    assert!(
        matches!(result, Err(ChangeError::Unreachable(_))),
        "{result:?}"
    );
    let state = handle.state_get().await;
    assert_eq!(state.locate.map(|l| l.ports), Some(vec![4]));

    sim.lock().unwrap().locate_status = 0;
    tokio::time::sleep(Duration::from_secs(31)).await;
    assert_eq!(scripts_count(&fake, LOCATE_OFF_SCRIPT), 1);
    assert!(handle.state_get().await.locate.is_none());
}

#[tokio::test(start_paused = true)]
async fn locate_refuses_bad_ports() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    for ports in [vec![], vec![13], vec![0], vec![4, 4]] {
        let result = handle.locate_run(ports, Duration::from_secs(60)).await;
        assert!(
            matches!(result, Err(ChangeError::Settings(_))),
            "{result:?}"
        );
    }
    assert_eq!(scripts_count(&fake, "port-locate"), 0);
}

#[tokio::test(start_paused = true)]
async fn locate_set_failure_keeps_no_state() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    sim.lock().unwrap().locate_status = 1;
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    let result = handle.locate_run(vec![1], Duration::from_secs(60)).await;
    match result {
        Err(ChangeError::ApplyFailed(text)) => assert!(text.contains("led driver busy"), "{text}"),
        other => panic!("expected ApplyFailed, got {other:?}"),
    }
    assert!(handle.state_get().await.locate.is_none());
}

// The end at the deadline restores the LEDs only, so it runs under a foreign lock too, and
// a failed end tries again at the next poll.
#[tokio::test(start_paused = true)]
async fn locate_end_ignores_lock_and_retries() {
    let sim = Arc::new(Mutex::new(Sim::xg10()));
    let fake = fake_new(sim.clone());
    let (_dir, store) = store_temp();
    let handle = spawn(&fake, ctx_xg10(), store, POLL, HOUR);
    tokio::time::sleep(Duration::from_secs(1)).await;
    handle
        .locate_run(vec![1], Duration::from_secs(20))
        .await
        .unwrap();
    {
        let mut sim = sim.lock().unwrap();
        sim.lock = FOREIGN_LOCK.into();
        sim.locate_status = 1;
    }
    tokio::time::sleep(Duration::from_secs(21)).await;
    assert_eq!(scripts_count(&fake, LOCATE_OFF_SCRIPT), 1);
    let state = handle.state_get().await;
    assert!(
        state.locate.as_ref().is_some_and(|l| l.end_failed),
        "{:?}",
        state.locate
    );
    assert!(
        state
            .last_failure
            .as_deref()
            .unwrap_or_default()
            .contains("led driver busy"),
        "{:?}",
        state.last_failure
    );

    // A later failure of another request stays the last failure: a retry that fails in the
    // same way does not write its text again.
    let result = handle.locate_run(vec![], Duration::from_secs(60)).await;
    assert!(
        matches!(result, Err(ChangeError::Settings(_))),
        "{result:?}"
    );
    tokio::time::sleep(POLL).await;
    assert_eq!(scripts_count(&fake, LOCATE_OFF_SCRIPT), 2);
    let state = handle.state_get().await;
    assert!(state.locate.as_ref().is_some_and(|l| l.end_failed));
    assert!(
        state
            .last_failure
            .as_deref()
            .unwrap_or_default()
            .contains("port list is empty"),
        "{:?}",
        state.last_failure
    );

    sim.lock().unwrap().locate_status = 0;
    tokio::time::sleep(POLL).await;
    assert_eq!(scripts_count(&fake, LOCATE_OFF_SCRIPT), 3);
    assert!(handle.state_get().await.locate.is_none());
    tokio::time::sleep(POLL * 3).await;
    assert_eq!(scripts_count(&fake, LOCATE_OFF_SCRIPT), 3);
}
