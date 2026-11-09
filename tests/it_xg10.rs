// Integration tests that change a real USW Pro XG 10; run with `--ignored`, USWCTL_IT_HOST and
// USWCTL_IT_USER. A PoE device on port 1 is the test load.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tempfile::TempDir;
use uswctl::backup::{BackupStore, stamp_format};
use uswctl::change::{Change, ChangeResult, SwitchCtx, change_apply};
use uswctl::config::lock_path_check;
use uswctl::live::PortLive;
use uswctl::model::Model;
use uswctl::overlay::{overlay_parse, overlay_render};
use uswctl::runcfg::{runcfg_overlay_check, runcfg_port_block_get, running_lines_clean};
use uswctl::settings::{
    BpduAction, EdgeMode, GlobalSettings, GlobalSettingsPatch, PoeMode, PortSettings,
    PortSettingsPatch, global_settings_read, port_settings_read,
};
use uswctl::syscfg::{syscfg_diff, syscfg_parse};
use uswctl::transport::{Output, SshTransport, Transport};
use uswctl::worker::{Action, DeviceHandle, DeviceState, Event, EventKind, worker_spawn};

const TOKEN: &str = "ubiquity-gui";
const FIRMWARE: &str = "7.5.15.17146";
const PORT_LOAD: u8 = 1;
const PORT_TEST: u8 = 3;
const PORT_UPLINK: u8 = 11;
const VLAN_NEW: u16 = 99;
const STP_PRIORITY_TEST: u32 = 61440;
const SYSCFG_PATH: &str = "/tmp/system.cfg";
const OVERLAY_PATH: &str = "/etc/persistent/uswctl-overlay.scr";
const OVERLAY_TMP_PATH: &str = "/tmp/uswctl-it-overlay.scr";
const NO_FILE_MARK: &str = "__IT_NOFILE__";
const RUNNING_SCRIPT: &str = "cli -c \"terminal length 0\" -c \"show running-config\"";
const APPLY_SCRIPT: &str = "syswrapper.sh apply-config";
const SAVE_SCRIPT: &str = "syswrapper.sh save-config && syswrapper.sh cfg_save_check";
const FLASH_READ_SCRIPT: &str = "rm -rf /tmp/uswctl-it-read && mkdir -p /tmp/uswctl-it-read && \
cfgmtd -r -p /tmp/uswctl-it-read -f /tmp/uswctl-it-read/read.cfg >&2 && \
cat /tmp/uswctl-it-read/read.cfg; rc=$?; rm -rf /tmp/uswctl-it-read; exit $rc";
const SSH_TIMEOUT: Duration = Duration::from_secs(120);
const POLL_FAST: Duration = Duration::from_secs(2);
const POLL_REBOOT: Duration = Duration::from_secs(10);
const OVERLAY_CHECK: Duration = Duration::from_secs(30);
const OVERLAY_CHECK_IDLE: Duration = Duration::from_secs(3600);
// mca-dump shows a link change 10 to 12 s late (docs/device-notes.md, section 3).
const LINK_WINDOW: Duration = Duration::from_secs(30);
const POE_OFF_WINDOW: Duration = Duration::from_secs(20);
const POE_ON_WINDOW: Duration = Duration::from_secs(90);
const POE_CYCLE_WINDOW: Duration = Duration::from_secs(180);
// The load boots after PoE comes back. The window allows a load that forwards traffic again
// about 60 s after the power, so that its MAC addresses show on port 1.
const LOAD_BACK_WINDOW: Duration = Duration::from_secs(240);
// `swctrl poe restart` gives the power back about 6 s after its start, and mca-dump lags,
// so the drop is sampled from `swctrl poe show` directly.
const CYCLE_SAMPLE_WINDOW: Duration = Duration::from_secs(15);
const REBOOT_WINDOW: Duration = Duration::from_secs(300);
const POE_POWER_ON_W: f32 = 1.0;

struct Start {
    syscfg: String,
    overlay: Option<String>,
}

struct It {
    t: Arc<SshTransport>,
    ctx: SwitchCtx,
    backup: BackupStore,
    start: Start,
    lock_taken: Mutex<bool>,
    _dir: TempDir,
}

fn host_get() -> String {
    std::env::var("USWCTL_IT_HOST").expect("set USWCTL_IT_HOST to the address of the XG 10")
}

fn user_get() -> String {
    std::env::var("USWCTL_IT_USER").expect("set USWCTL_IT_USER to the SSH user of the XG 10")
}

// Without USWCTL_IT_LOCK_PATH the tests take no lock, as the daemon checks none. The path
// goes into shell scripts, so it gets the check of the daemon config.
fn lock_path_get() -> Option<String> {
    let path = std::env::var("USWCTL_IT_LOCK_PATH")
        .ok()
        .filter(|p| !p.is_empty())?;
    lock_path_check(&path).expect("USWCTL_IT_LOCK_PATH");
    Some(path)
}

fn key_path_get() -> PathBuf {
    if let Ok(path) = std::env::var("USWCTL_IT_KEY") {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME").expect("HOME is not set");
    PathBuf::from(home).join(".ssh/id_rsa")
}

fn ctx_build() -> SwitchCtx {
    SwitchCtx {
        name: "xg10".to_string(),
        model: Model::Xg10,
        lock_path: lock_path_get(),
        lock_token: TOKEN.to_string(),
        tested_firmware: vec![FIRMWARE.to_string()],
    }
}

async fn script_ok(t: &dyn Transport, script: &str) -> String {
    let output = t
        .script_run(script)
        .await
        .unwrap_or_else(|e| panic!("{script}: {e:#}"));
    assert_eq!(
        output.status, 0,
        "{script}: exit status {}: {} {}",
        output.status, output.stdout, output.stderr
    );
    output.stdout
}

async fn file_read_optional(t: &dyn Transport, path: &str) -> Option<String> {
    let script = format!("if [ -e {path} ]; then cat {path}; else echo {NO_FILE_MARK}; fi");
    let text = script_ok(t, &script).await;
    if text.trim_end() == NO_FILE_MARK {
        return None;
    }
    Some(text)
}

async fn syscfg_now(t: &dyn Transport) -> String {
    script_ok(t, &format!("cat {SYSCFG_PATH}")).await
}

async fn overlay_now(t: &dyn Transport) -> Option<String> {
    file_read_optional(t, OVERLAY_PATH).await
}

async fn running_now(t: &dyn Transport) -> String {
    script_ok(t, RUNNING_SCRIPT).await
}

fn time_iso_get() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs();
    stamp_format(secs)
}

// Returns true if this call wrote the lock. A lock of another holder stops the test.
async fn lock_take(t: &dyn Transport) -> bool {
    let Some(lock_path) = lock_path_get() else {
        return false;
    };
    let script = format!(
        "if [ -s {lock_path} ]; then cat {lock_path}; exit 3; fi; \
echo '{TOKEN} {} uswctl integration tests' > {lock_path}",
        time_iso_get()
    );
    let output = t
        .script_run(&script)
        .await
        .unwrap_or_else(|e| panic!("lock take: {e:#}"));
    match output.status {
        0 => true,
        3 if output.stdout.contains(TOKEN) => false,
        3 => panic!("another holder has the lock: {}", output.stdout.trim()),
        status => panic!("lock take: exit status {status}: {}", output.stderr),
    }
}

async fn it_setup() -> It {
    let dir = tempfile::tempdir().unwrap();
    let t = Arc::new(SshTransport {
        host: host_get(),
        user: user_get(),
        key_path: key_path_get(),
        control_dir: dir.path().join("ssh"),
        timeout: SSH_TIMEOUT,
    });
    let lock_taken = lock_take(&*t).await;
    let start = Start {
        syscfg: syscfg_now(&*t).await,
        overlay: overlay_now(&*t).await,
    };
    It {
        t,
        ctx: ctx_build(),
        backup: BackupStore {
            dir: dir.path().join("backups"),
        },
        start,
        lock_taken: Mutex::new(lock_taken),
        _dir: dir,
    }
}

async fn ssh_wait(t: &dyn Transport, limit: Duration) {
    let begin = Instant::now();
    loop {
        if let Ok(output) = t.script_run("true").await
            && output.status == 0
        {
            return;
        }
        assert!(begin.elapsed() < limit, "no SSH answer within {limit:?}");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// A failed test can leave port 1 shut down or without PoE, so the restore checks both.
async fn load_port_restore(t: &dyn Transport) {
    let block = runcfg_port_block_get(&running_now(t).await, "te1");
    if block.iter().any(|l| l == " shutdown") {
        eprintln!("restore: te1 had shutdown; sending no shutdown");
        script_ok(
            t,
            "printf 'interface te1\\n no shutdown\\nexit\\n' > /tmp/uswctl-it-link.scr && \
cli -e /tmp/uswctl-it-link.scr; rc=$?; rm -f /tmp/uswctl-it-link.scr; exit $rc",
        )
        .await;
    }
    let poe = script_ok(t, &format!("swctrl poe show id {PORT_LOAD}")).await;
    if poe_opmode_get(&poe, PORT_LOAD).as_deref() == Some("Off") {
        eprintln!("restore: port 1 PoE was off; setting auto");
        script_ok(t, &format!("swctrl poe set auto id {PORT_LOAD}")).await;
    }
}

// Puts back the exact start files: a revert through the settings layer can move a key
// that it removed and wrote again to the end of its group.
async fn it_restore(it: &It) {
    let t = &*it.t;
    ssh_wait(t, REBOOT_WINDOW).await;
    if lock_take(t).await {
        *it.lock_taken.lock().unwrap() = true;
    }
    load_port_restore(t).await;
    let mut save = false;
    let overlay = overlay_now(t).await;
    if overlay != it.start.overlay {
        match &it.start.overlay {
            None => script_ok(t, &format!("rm -f {OVERLAY_PATH}")).await,
            Some(text) => {
                t.file_put(OVERLAY_PATH, text.as_bytes()).await.unwrap();
                script_ok(t, &format!("cli -e {OVERLAY_PATH}")).await
            }
        };
        eprintln!("restore: overlay file set back to its start state");
        save = true;
    }
    if syscfg_now(t).await != it.start.syscfg {
        t.file_put(SYSCFG_PATH, it.start.syscfg.as_bytes())
            .await
            .unwrap();
        script_ok(t, APPLY_SCRIPT).await;
        eprintln!("restore: system.cfg set back to its start text");
        save = true;
    }
    if save {
        script_ok(t, SAVE_SCRIPT).await;
    }
    // A removed overlay file leaves its commands in the running config, for example after
    // a failure between the overlay repair and the revert.
    let start_overlay = overlay_parse(&it.start.overlay.clone().unwrap_or_default(), Model::Xg10);
    if !runcfg_overlay_check(&running_now(t).await, &start_overlay, Model::Xg10) {
        let text = overlay_render(&start_overlay, Model::Xg10);
        t.file_put(OVERLAY_TMP_PATH, text.as_bytes()).await.unwrap();
        script_ok(
            t,
            &format!("cli -e {OVERLAY_TMP_PATH}; rc=$?; rm -f {OVERLAY_TMP_PATH}; exit $rc"),
        )
        .await;
        eprintln!("restore: running config set back to the start overlay");
    }
    assert!(
        runcfg_overlay_check(&running_now(t).await, &start_overlay, Model::Xg10),
        "running config after restore does not match the start overlay"
    );
    assert_eq!(
        syscfg_now(t).await,
        it.start.syscfg,
        "system.cfg after restore"
    );
    // A save in the first minutes after a boot returned 0 and left the flash unchanged.
    let flash = script_ok(t, FLASH_READ_SCRIPT).await;
    assert_eq!(
        flash.trim_end_matches('\n'),
        it.start.syscfg.trim_end_matches('\n'),
        "flash after restore"
    );
    assert_eq!(
        overlay_now(t).await,
        it.start.overlay,
        "overlay after restore"
    );
    let lock_taken = *it.lock_taken.lock().unwrap();
    if let Some(lock_path) = lock_path_get().filter(|_| lock_taken) {
        script_ok(t, &format!("rm -f {lock_path}")).await;
    }
}

// The body runs in its own task, so that a failed assertion still reaches the restore.
async fn it_run<F, Fut>(name: &str, body: F)
where
    F: FnOnce(Arc<It>) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let begin = Instant::now();
    let it = Arc::new(it_setup().await);
    let result = tokio::spawn(body(it.clone())).await;
    it_restore(&it).await;
    eprintln!("{name}: run time {:.1} s", begin.elapsed().as_secs_f64());
    if let Err(e) = result {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn apply(it: &It, change: Change) -> ChangeResult {
    let result = change_apply(&*it.t, &it.ctx, &it.backup, &change)
        .await
        .unwrap_or_else(|e| panic!("{change:?}: {e}"));
    assert!(result.readback_ok, "{change:?}: readback_ok false");
    assert!(
        !result.keys.is_empty() || result.overlay_changed,
        "{change:?}: the change wrote nothing"
    );
    result
}

fn port_change(patch: PortSettingsPatch) -> Change {
    Change::Port {
        idx: PORT_TEST,
        patch,
    }
}

async fn state_files_get(t: &dyn Transport) -> (String, String) {
    (
        syscfg_now(t).await,
        overlay_now(t).await.unwrap_or_default(),
    )
}

async fn port_settings_now(t: &dyn Transport, idx: u8) -> PortSettings {
    let (cfg, overlay) = state_files_get(t).await;
    port_settings_read(
        &syscfg_parse(&cfg),
        &overlay_parse(&overlay, Model::Xg10),
        idx,
    )
}

async fn global_settings_now(t: &dyn Transport) -> GlobalSettings {
    let (cfg, overlay) = state_files_get(t).await;
    global_settings_read(&syscfg_parse(&cfg), &overlay_parse(&overlay, Model::Xg10))
}

fn start_port_settings(it: &It, idx: u8) -> PortSettings {
    let overlay = it.start.overlay.clone().unwrap_or_default();
    port_settings_read(
        &syscfg_parse(&it.start.syscfg),
        &overlay_parse(&overlay, Model::Xg10),
        idx,
    )
}

fn start_global_settings(it: &It) -> GlobalSettings {
    let overlay = it.start.overlay.clone().unwrap_or_default();
    global_settings_read(
        &syscfg_parse(&it.start.syscfg),
        &overlay_parse(&overlay, Model::Xg10),
    )
}

// Key order can differ after a revert (see `it_restore`), so the start state is compared
// key by key here; `it_restore` checks the exact text.
async fn start_state_check(it: &It, step: &str) {
    let (cfg, overlay) = state_files_get(&*it.t).await;
    let keys = syscfg_diff(&syscfg_parse(&it.start.syscfg), &syscfg_parse(&cfg));
    assert!(
        keys.is_empty(),
        "{step}: keys differ from the start: {keys:?}"
    );
    let start_overlay = overlay_parse(&it.start.overlay.clone().unwrap_or_default(), Model::Xg10);
    assert_eq!(
        overlay_parse(&overlay, Model::Xg10),
        start_overlay,
        "{step}: overlay differs from the start"
    );
}

async fn port_block_now(t: &dyn Transport, cli_name: &str) -> Vec<String> {
    runcfg_port_block_get(&running_now(t).await, cli_name)
}

fn vlan_list_parse(text: &str) -> BTreeSet<u16> {
    let mut ids = BTreeSet::new();
    for part in text.split(',') {
        match part.split_once('-') {
            Some((low, high)) => {
                if let (Ok(low), Ok(high)) = (low.trim().parse::<u16>(), high.trim().parse()) {
                    ids.extend(low..=high);
                }
            }
            None => ids.extend(part.trim().parse::<u16>().ok()),
        }
    }
    ids
}

fn block_vlans_get(block: &[String], prefix: &str, suffix: &str) -> BTreeSet<u16> {
    block
        .iter()
        .filter_map(|l| l.strip_prefix(prefix)?.strip_suffix(suffix))
        .flat_map(vlan_list_parse)
        .collect()
}

fn block_allowed_get(block: &[String]) -> BTreeSet<u16> {
    let mut ids = block_vlans_get(block, " switchport hybrid allowed vlan add ", " tagged");
    ids.extend(block_vlans_get(
        block,
        " switchport hybrid allowed vlan add ",
        " untagged",
    ));
    ids
}

fn poe_opmode_get(poe_show: &str, idx: u8) -> Option<String> {
    let idx_text = idx.to_string();
    poe_show.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next() == Some(idx_text.as_str()))
            .then(|| fields.next().map(str::to_string))
            .flatten()
    })
}

// The class column can hold two words ("Class 6"), so the power is read from the end:
// Power(W), Voltage(V), Current(mA).
fn poe_power_get(poe_show: &str, idx: u8) -> Option<f32> {
    let idx_text = idx.to_string();
    poe_show.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.first() != Some(&idx_text.as_str()) || fields.len() < 4 {
            return None;
        }
        fields[fields.len() - 3].parse().ok()
    })
}

fn port_live_get(state: &DeviceState, idx: u8) -> Option<&PortLive> {
    state.ports.iter().find(|p| p.idx == idx).map(|p| &p.live)
}

fn poe_off_check(live: &PortLive) -> bool {
    live.poe_power_w.is_none_or(|w| w == 0.0)
}

fn poe_on_check(live: &PortLive) -> bool {
    live.poe_power_w.is_some_and(|w| w > POE_POWER_ON_W)
}

// Records each call of the worker with its start time and duration, so that a failed
// reboot test shows which SSH call the worker waited for.
struct TracedTransport {
    inner: Arc<SshTransport>,
    begin: Instant,
    trace: Arc<Mutex<Vec<String>>>,
}

impl TracedTransport {
    fn note(&self, text: String) {
        let at = self.begin.elapsed().as_secs_f64();
        self.trace
            .lock()
            .unwrap()
            .push(format!("{at:.1} s: {text}"));
    }
}

fn result_describe<T>(result: &anyhow::Result<T>, status: impl Fn(&T) -> String) -> String {
    match result {
        Ok(value) => status(value),
        Err(e) => format!("error {e:#}"),
    }
}

#[async_trait]
impl Transport for TracedTransport {
    async fn script_run(&self, script: &str) -> anyhow::Result<Output> {
        let head = script.lines().next().unwrap_or_default().to_string();
        self.note(format!("start {head}"));
        let call_begin = Instant::now();
        let result = self.inner.script_run(script).await;
        let what = result_describe(&result, |o| format!("exit {}", o.status));
        self.note(format!(
            "end after {:.1} s, {what}: {head}",
            call_begin.elapsed().as_secs_f64()
        ));
        result
    }

    async fn master_reset(&self) {
        self.note("master_reset".to_string());
        self.inner.master_reset().await;
    }

    async fn file_put(&self, path: &str, data: &[u8]) -> anyhow::Result<()> {
        self.note(format!("start file_put {path}"));
        let call_begin = Instant::now();
        let result = self.inner.file_put(path, data).await;
        let what = result_describe(&result, |_| "ok".to_string());
        self.note(format!(
            "end after {:.1} s, {what}: file_put {path}",
            call_begin.elapsed().as_secs_f64()
        ));
        result
    }
}

fn worker_build(it: &It, poll: Duration, overlay_check: Duration) -> DeviceHandle {
    let t: Arc<dyn Transport> = it.t.clone();
    worker_t_build(it, t, poll, overlay_check)
}

fn worker_t_build(
    it: &It,
    t: Arc<dyn Transport>,
    poll: Duration,
    overlay_check: Duration,
) -> DeviceHandle {
    worker_spawn(
        t,
        it.ctx.clone(),
        vec![PORT_UPLINK],
        it.backup.clone(),
        poll,
        overlay_check,
    )
    .unwrap()
}

async fn state_wait(
    handle: &DeviceHandle,
    limit: Duration,
    what: &str,
    check: impl Fn(&DeviceState) -> bool,
) -> Duration {
    let begin = Instant::now();
    loop {
        let state = handle.state_get().await;
        if check(&state) {
            return begin.elapsed();
        }
        assert!(
            begin.elapsed() < limit,
            "{what}: not within {limit:?}; port 1: {:?}; last failure: {:?}",
            port_live_get(&state, PORT_LOAD),
            state.last_failure
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn worker_ready_wait(handle: &DeviceHandle) {
    state_wait(handle, Duration::from_secs(60), "first poll", |s| {
        port_live_get(s, PORT_LOAD).is_some()
    })
    .await;
}

async fn load_macs_get(handle: &DeviceHandle) -> Vec<String> {
    let state = handle.state_get().await;
    let macs = port_live_get(&state, PORT_LOAD)
        .map(|p| p.macs.clone())
        .unwrap_or_default();
    assert!(!macs.is_empty(), "port 1 has no MAC address at the start");
    macs
}

// A load can send from one MAC address a few seconds after the power comes back, long
// before it has booted, so one start MAC address is not enough. Its other MAC addresses
// show only when the load itself sends, which it can skip for minutes, so not all are
// needed.
async fn load_back_wait(handle: &DeviceHandle, step: &str, macs_start: &[String]) {
    let distinct: BTreeSet<&String> = macs_start.iter().collect();
    let needed = distinct.len().min(2);
    let took = state_wait(handle, LOAD_BACK_WINDOW, step, |s| {
        port_live_get(s, PORT_LOAD).is_some_and(|p| {
            let back = distinct.iter().filter(|m| p.macs.contains(m)).count();
            p.up && poe_on_check(p) && back >= needed
        })
    })
    .await;
    let state = handle.state_get().await;
    eprintln!(
        "{step}: load back after {:.1} s: {:?}",
        took.as_secs_f64(),
        port_live_get(&state, PORT_LOAD)
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn it_settings_roundtrip() {
    it_run("it_settings_roundtrip", |it| async move {
        let s0 = start_port_settings(&it, PORT_TEST);
        let table: Vec<u16> = start_global_settings(&it).vlans;
        let others: Vec<u16> = table
            .iter()
            .copied()
            .filter(|id| *id != 1 && Some(*id) != s0.native_vlan && !s0.tagged_vlans.contains(id))
            .collect();
        assert!(
            others.len() >= 3,
            "the VLAN table has too few VLANs: {table:?}"
        );
        let name = "uswctl-it+3".to_string();
        let steps: Vec<(&str, PortSettingsPatch, PortSettingsPatch)> = vec![
            (
                "name",
                PortSettingsPatch {
                    name: Some(name.clone()),
                    ..Default::default()
                },
                PortSettingsPatch {
                    name: Some(s0.name.clone()),
                    ..Default::default()
                },
            ),
            (
                "native_vlan",
                PortSettingsPatch {
                    native_vlan: Some(Some(others[0])),
                    ..Default::default()
                },
                PortSettingsPatch {
                    native_vlan: Some(s0.native_vlan),
                    ..Default::default()
                },
            ),
            (
                "tagged_vlans",
                PortSettingsPatch {
                    tagged_vlans: Some(vec![others[1], others[2]]),
                    ..Default::default()
                },
                PortSettingsPatch {
                    tagged_vlans: Some(s0.tagged_vlans.clone()),
                    ..Default::default()
                },
            ),
            (
                "poe",
                PortSettingsPatch {
                    poe: Some(match s0.poe {
                        PoeMode::Auto => PoeMode::Off,
                        PoeMode::Off => PoeMode::Auto,
                    }),
                    ..Default::default()
                },
                PortSettingsPatch {
                    poe: Some(s0.poe),
                    ..Default::default()
                },
            ),
            (
                "stp",
                PortSettingsPatch {
                    stp: Some(!s0.stp),
                    ..Default::default()
                },
                PortSettingsPatch {
                    stp: Some(s0.stp),
                    ..Default::default()
                },
            ),
            (
                "edge enabled",
                PortSettingsPatch {
                    edge: Some(EdgeMode::Enabled),
                    ..Default::default()
                },
                PortSettingsPatch {
                    edge: Some(s0.edge),
                    ..Default::default()
                },
            ),
            (
                "edge disabled",
                PortSettingsPatch {
                    edge: Some(EdgeMode::Disabled),
                    ..Default::default()
                },
                PortSettingsPatch {
                    edge: Some(s0.edge),
                    ..Default::default()
                },
            ),
            (
                "bpdu_guard",
                PortSettingsPatch {
                    bpdu_guard: Some(!s0.bpdu_guard),
                    ..Default::default()
                },
                PortSettingsPatch {
                    bpdu_guard: Some(s0.bpdu_guard),
                    ..Default::default()
                },
            ),
            (
                "bpdu_filter",
                PortSettingsPatch {
                    bpdu_filter: Some(!s0.bpdu_filter),
                    ..Default::default()
                },
                PortSettingsPatch {
                    bpdu_filter: Some(s0.bpdu_filter),
                    ..Default::default()
                },
            ),
            (
                "enabled",
                PortSettingsPatch {
                    enabled: Some(!s0.enabled),
                    ..Default::default()
                },
                PortSettingsPatch {
                    enabled: Some(s0.enabled),
                    ..Default::default()
                },
            ),
        ];
        for (label, patch, revert) in steps {
            let begin = Instant::now();
            let result = apply(&it, port_change(patch)).await;
            eprintln!(
                "settings {label}: applied in {:.1} s, keys {:?}, cli {:?}",
                begin.elapsed().as_secs_f64(),
                result.keys,
                result.cli_lines
            );
            eprintln!(
                "settings {label}: te3 {:?}",
                port_block_now(&*it.t, "te3").await
            );
            if label == "name" {
                let flash = script_ok(&*it.t, FLASH_READ_SCRIPT).await;
                let line = format!("switch.port.{PORT_TEST}.name={name}");
                assert!(
                    flash.lines().any(|l| l == line),
                    "the flash read-back has no line {line:?}"
                );
            }
            apply(&it, port_change(revert)).await;
            assert_eq!(
                port_settings_now(&*it.t, PORT_TEST).await,
                s0,
                "settings {label}: port 3 after the revert"
            );
        }
        assert_eq!(
            syscfg_now(&*it.t).await,
            it.start.syscfg,
            "system.cfg after all reverts"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn it_bpdu_overlay() {
    it_run("it_bpdu_overlay", |it| async move {
        let g0 = start_global_settings(&it);
        let s0 = start_port_settings(&it, PORT_TEST);
        assert_eq!(g0.bpdu_action, BpduAction::Flooding, "start global action");
        assert!(!s0.bpdu_filter, "port 3 has a filter at the start");

        apply(
            &it,
            Change::Global {
                patch: GlobalSettingsPatch {
                    bpdu_action: Some(BpduAction::Filtering),
                    ..Default::default()
                },
            },
        )
        .await;
        apply(
            &it,
            port_change(PortSettingsPatch {
                bpdu_filter: Some(true),
                ..Default::default()
            }),
        )
        .await;
        let running = running_now(&*it.t).await;
        let lines = running_lines_clean(&running);
        assert!(
            lines.iter().any(|l| l == "spanning-tree bpdu filtering"),
            "no global filtering line"
        );
        let block = runcfg_port_block_get(&running, "te3");
        assert!(
            block.iter().any(|l| l == " spanning-tree bpdu-filter"),
            "te3 has no filter: {block:?}"
        );
        let stp_show = script_ok(&*it.t, "cli -c \"show spanning-tree interfaces te3\"").await;
        assert!(
            stp_show.contains("BPDU Filter: Enabled"),
            "show spanning-tree te3: {stp_show}"
        );

        apply(
            &it,
            port_change(PortSettingsPatch {
                bpdu_filter: Some(false),
                ..Default::default()
            }),
        )
        .await;
        apply(
            &it,
            Change::Global {
                patch: GlobalSettingsPatch {
                    bpdu_action: Some(BpduAction::Flooding),
                    ..Default::default()
                },
            },
        )
        .await;
        let running = running_now(&*it.t).await;
        let lines = running_lines_clean(&running);
        assert!(
            !lines.iter().any(|l| l.contains("bpdu")),
            "a bpdu line is left: {:?}",
            lines
                .iter()
                .filter(|l| l.contains("bpdu"))
                .collect::<Vec<_>>()
        );
        start_state_check(&it, "bpdu overlay").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn it_vlan_add_remove() {
    it_run("it_vlan_add_remove", |it| async move {
        let g0 = start_global_settings(&it);
        assert!(
            !g0.vlans.contains(&VLAN_NEW),
            "VLAN {VLAN_NEW} exists at the start"
        );
        let all_ports: Vec<u8> = (7..=PORT_UPLINK)
            .filter(|idx| start_port_settings(&it, *idx).vlan_all)
            .collect();
        assert!(
            !all_ports.is_empty(),
            "no port of 7 to 11 has the all-VLANs profile"
        );
        assert!(
            !start_port_settings(&it, PORT_TEST).vlan_all,
            "port 3 is an all-VLANs port"
        );

        let mut vlans = g0.vlans.clone();
        vlans.push(VLAN_NEW);
        let result = apply(
            &it,
            Change::Global {
                patch: GlobalSettingsPatch {
                    vlans: Some(vlans),
                    ..Default::default()
                },
            },
        )
        .await;
        eprintln!("vlan add: {} keys", result.keys.len());
        let running = running_now(&*it.t).await;
        for idx in &all_ports {
            let block = runcfg_port_block_get(&running, &format!("te{idx}"));
            let tagged = block_vlans_get(&block, " switchport hybrid allowed vlan add ", " tagged");
            eprintln!("vlan add: te{idx} {block:?}");
            assert!(
                tagged.contains(&VLAN_NEW),
                "te{idx} does not carry VLAN {VLAN_NEW} tagged: {block:?}"
            );
        }
        let block = runcfg_port_block_get(&running, "te3");
        eprintln!("vlan add: te3 {block:?}");
        assert!(
            !block_allowed_get(&block).contains(&VLAN_NEW),
            "te3 carries VLAN {VLAN_NEW}: {block:?}"
        );
        let lines = running_lines_clean(&running);
        eprintln!(
            "vlan add: vlan lines {:?}",
            lines
                .iter()
                .filter(|l| l.starts_with("vlan "))
                .collect::<Vec<_>>()
        );

        apply(
            &it,
            Change::Global {
                patch: GlobalSettingsPatch {
                    vlans: Some(g0.vlans.clone()),
                    ..Default::default()
                },
            },
        )
        .await;
        assert_eq!(
            syscfg_now(&*it.t).await,
            it.start.syscfg,
            "system.cfg after the VLAN removal"
        );
        let running = running_now(&*it.t).await;
        for idx in &all_ports {
            let block = runcfg_port_block_get(&running, &format!("te{idx}"));
            assert!(
                !block_allowed_get(&block).contains(&VLAN_NEW),
                "te{idx} still carries VLAN {VLAN_NEW}: {block:?}"
            );
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn it_effect_forms() {
    it_run("it_effect_forms", |it| async move {
        let g0 = start_global_settings(&it);
        let s0 = start_port_settings(&it, PORT_TEST);

        apply(
            &it,
            Change::Global {
                patch: GlobalSettingsPatch {
                    stp_priority: Some(STP_PRIORITY_TEST),
                    ..Default::default()
                },
            },
        )
        .await;
        assert_eq!(
            global_settings_now(&*it.t).await.stp_priority,
            STP_PRIORITY_TEST,
            "priority in system.cfg"
        );
        let lines = running_lines_clean(&running_now(&*it.t).await);
        eprintln!(
            "forms priority: {:?}",
            lines
                .iter()
                .filter(|l| l.starts_with("spanning-tree"))
                .collect::<Vec<_>>()
        );
        apply(
            &it,
            Change::Global {
                patch: GlobalSettingsPatch {
                    stp_priority: Some(g0.stp_priority),
                    ..Default::default()
                },
            },
        )
        .await;
        start_state_check(&it, "priority").await;

        assert!(s0.stp, "port 3 has STP off at the start");
        apply(
            &it,
            port_change(PortSettingsPatch {
                stp: Some(false),
                ..Default::default()
            }),
        )
        .await;
        apply(
            &it,
            port_change(PortSettingsPatch {
                edge: Some(EdgeMode::Enabled),
                ..Default::default()
            }),
        )
        .await;
        eprintln!(
            "forms edge with STP off: te3 {:?}",
            port_block_now(&*it.t, "te3").await
        );
        apply(
            &it,
            port_change(PortSettingsPatch {
                bpdu_guard: Some(true),
                ..Default::default()
            }),
        )
        .await;
        eprintln!(
            "forms BPDU guard with STP off: te3 {:?}",
            port_block_now(&*it.t, "te3").await
        );
        apply(
            &it,
            port_change(PortSettingsPatch {
                bpdu_guard: Some(s0.bpdu_guard),
                edge: Some(s0.edge),
                ..Default::default()
            }),
        )
        .await;
        apply(
            &it,
            port_change(PortSettingsPatch {
                stp: Some(s0.stp),
                ..Default::default()
            }),
        )
        .await;
        start_state_check(&it, "edge and BPDU guard with STP off").await;

        let native = s0
            .native_vlan
            .expect("port 3 has a native VLAN at the start");
        apply(
            &it,
            port_change(PortSettingsPatch {
                native_vlan: Some(None),
                tagged_vlans: Some(vec![native]),
                ..Default::default()
            }),
        )
        .await;
        eprintln!(
            "forms no native VLAN: te3 {:?}",
            port_block_now(&*it.t, "te3").await
        );
        let now = port_settings_now(&*it.t, PORT_TEST).await;
        assert_eq!(now.native_vlan, None, "native VLAN after the removal");
        assert_eq!(
            now.tagged_vlans,
            vec![native],
            "tagged VLANs after the removal"
        );
        apply(
            &it,
            port_change(PortSettingsPatch {
                native_vlan: Some(s0.native_vlan),
                tagged_vlans: Some(s0.tagged_vlans.clone()),
                ..Default::default()
            }),
        )
        .await;
        start_state_check(&it, "no native VLAN").await;

        apply(
            &it,
            port_change(PortSettingsPatch {
                name: Some(String::new()),
                ..Default::default()
            }),
        )
        .await;
        eprintln!(
            "forms empty name: te3 {:?}",
            port_block_now(&*it.t, "te3").await
        );
        apply(
            &it,
            port_change(PortSettingsPatch {
                name: Some(s0.name.clone()),
                ..Default::default()
            }),
        )
        .await;
        start_state_check(&it, "empty name").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn it_link_down_up_port1() {
    it_run("it_link_down_up_port1", |it| async move {
        let handle = worker_build(&it, POLL_FAST, OVERLAY_CHECK_IDLE);
        worker_ready_wait(&handle).await;
        let state = handle.state_get().await;
        assert!(
            port_live_get(&state, PORT_LOAD).is_some_and(|p| p.up),
            "port 1 has no link at the start"
        );

        handle
            .action_run(PORT_LOAD, Action::LinkDown)
            .await
            .unwrap();
        let took = state_wait(&handle, LINK_WINDOW, "link down", |s| {
            port_live_get(s, PORT_LOAD).is_some_and(|p| !p.up)
        })
        .await;
        eprintln!("link down seen after {:.1} s", took.as_secs_f64());
        let state = handle.state_get().await;
        assert_eq!(
            state.transient.get(&PORT_LOAD),
            Some(&vec![Action::LinkDown]),
            "transient list after link down"
        );

        handle.action_run(PORT_LOAD, Action::LinkUp).await.unwrap();
        let took = state_wait(&handle, LINK_WINDOW, "link up", |s| {
            port_live_get(s, PORT_LOAD).is_some_and(|p| p.up)
        })
        .await;
        eprintln!("link up seen after {:.1} s", took.as_secs_f64());
        let state = handle.state_get().await;
        assert!(state.transient.is_empty(), "transient list after link up");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn it_poe_off_on_port1() {
    it_run("it_poe_off_on_port1", |it| async move {
        let handle = worker_build(&it, POLL_FAST, OVERLAY_CHECK_IDLE);
        worker_ready_wait(&handle).await;
        let macs_start = load_macs_get(&handle).await;
        let state = handle.state_get().await;
        assert!(
            port_live_get(&state, PORT_LOAD).is_some_and(poe_on_check),
            "port 1 has no PoE power at the start"
        );

        handle.action_run(PORT_LOAD, Action::PoeOff).await.unwrap();
        let took = state_wait(&handle, POE_OFF_WINDOW, "PoE off", |s| {
            port_live_get(s, PORT_LOAD).is_some_and(poe_off_check)
        })
        .await;
        eprintln!("PoE off seen after {:.1} s", took.as_secs_f64());

        handle.action_run(PORT_LOAD, Action::PoeOn).await.unwrap();
        let took = state_wait(&handle, POE_ON_WINDOW, "PoE on", |s| {
            port_live_get(s, PORT_LOAD).is_some_and(poe_on_check)
        })
        .await;
        eprintln!("PoE on seen after {:.1} s", took.as_secs_f64());
        assert!(
            handle.state_get().await.transient.is_empty(),
            "transient list after PoE on"
        );
        load_back_wait(&handle, "PoE off and on", &macs_start).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn it_poe_cycle_port1() {
    it_run("it_poe_cycle_port1", |it| async move {
        let handle = worker_build(&it, POLL_FAST, OVERLAY_CHECK_IDLE);
        worker_ready_wait(&handle).await;
        let macs_start = load_macs_get(&handle).await;
        let state = handle.state_get().await;
        assert!(
            port_live_get(&state, PORT_LOAD).is_some_and(poe_on_check),
            "port 1 has no PoE power at the start"
        );

        let begin = Instant::now();
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let sampler = {
            let t = it.t.clone();
            let seen = seen.clone();
            tokio::spawn(async move {
                let sample_begin = Instant::now();
                while sample_begin.elapsed() < CYCLE_SAMPLE_WINDOW {
                    if let Ok(output) = t
                        .script_run(&format!("swctrl poe show id {PORT_LOAD}"))
                        .await
                        && poe_power_get(&output.stdout, PORT_LOAD) == Some(0.0)
                    {
                        seen.lock().unwrap().push(format!(
                            "swctrl power 0.00 at {:.1} s",
                            sample_begin.elapsed().as_secs_f64()
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            })
        };
        handle
            .action_run(PORT_LOAD, Action::PoeCycle)
            .await
            .unwrap();
        let took = state_wait(&handle, POE_CYCLE_WINDOW, "PoE cycle", |s| {
            if let Some(p) = port_live_get(s, PORT_LOAD) {
                if poe_off_check(p) || !p.up {
                    seen.lock()
                        .unwrap()
                        .push(format!("worker: power {:?} up {}", p.poe_power_w, p.up));
                }
                let dropped = !seen.lock().unwrap().is_empty();
                return dropped && begin.elapsed() > CYCLE_SAMPLE_WINDOW && p.up && poe_on_check(p);
            }
            false
        })
        .await;
        sampler.await.unwrap();
        let seen = seen.lock().unwrap().clone();
        eprintln!(
            "PoE cycle: power back after {:.1} s; drop seen as: first {:?}, last {:?}, {} samples",
            took.as_secs_f64(),
            seen.first(),
            seen.last(),
            seen.len()
        );
        assert!(!seen.is_empty(), "no power drop seen");
        load_back_wait(&handle, "PoE cycle", &macs_start).await;
    })
    .await;
}

fn event_find(state: &DeviceState, kind: &EventKind, after: SystemTime) -> Option<Event> {
    state
        .events
        .iter()
        .find(|e| e.kind == *kind && e.at >= after)
        .cloned()
}

async fn uptime_read(t: &dyn Transport) -> Option<f64> {
    let output = t.script_run("cat /proc/uptime").await.ok()?;
    if output.status != 0 {
        return None;
    }
    output.stdout.split_whitespace().next()?.parse().ok()
}

// `cfg_save_check` writes the flash only when the management file is not empty.
const SAVE_STATE_SCRIPT: &str = "cut -d ' ' -f 1 /proc/uptime; \
if [ -s /etc/persistent/cfg/mgmt ]; then echo mgmt=ready; else echo mgmt=empty; fi; \
echo state=$(cat /var/run/system.state 2>/dev/null)";

async fn save_state_read(t: &dyn Transport) -> Option<String> {
    let output = t.script_run(SAVE_STATE_SCRIPT).await.ok()?;
    let mut lines = output.stdout.lines();
    let uptime = lines.next()?.to_string();
    Some(format!(
        "uptime {uptime} s, {}",
        lines.collect::<Vec<_>>().join(", ")
    ))
}

// Records each change of the save readiness, so that a failed flash save after a boot
// shows the state of the switch at that time.
fn save_state_watch_spawn(
    t: Arc<SshTransport>,
    log: Arc<Mutex<Vec<String>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last = String::new();
        loop {
            if let Some(text) = save_state_read(&*t).await {
                let state = text.split_once(", ").map(|(_, rest)| rest.to_string());
                if state.as_deref() != Some(last.as_str()) {
                    last = state.unwrap_or_default();
                    log.lock().unwrap().push(text);
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn it_reboot_persistence() {
    it_run("it_reboot_persistence", |it| async move {
        let t = &*it.t;
        let s0 = start_port_settings(&it, PORT_TEST);
        assert!(s0.enabled, "port 3 is disabled at the start");
        let native_new = start_global_settings(&it)
            .vlans
            .into_iter()
            .find(|id| *id != 1 && Some(*id) != s0.native_vlan && !s0.tagged_vlans.contains(id))
            .expect("a second VLAN in the table");
        let name = "uswctl-boot".to_string();
        let trace: Arc<Mutex<Vec<String>>> = Arc::default();
        let traced: Arc<dyn Transport> = Arc::new(TracedTransport {
            inner: it.t.clone(),
            begin: Instant::now(),
            trace: trace.clone(),
        });
        let handle = worker_t_build(&it, traced, POLL_REBOOT, OVERLAY_CHECK);
        worker_ready_wait(&handle).await;
        let macs_start = load_macs_get(&handle).await;

        let result = handle
            .change_run(
                port_change(PortSettingsPatch {
                    name: Some(name.clone()),
                    edge: Some(EdgeMode::Enabled),
                    native_vlan: Some(Some(native_new)),
                    bpdu_filter: Some(true),
                    enabled: Some(false),
                    ..Default::default()
                }),
                false,
            )
            .await
            .unwrap();
        assert!(result.readback_ok, "readback_ok of the boot settings");
        let uptime_before = uptime_read(t).await.expect("uptime before the reboot");
        let reboot_at = SystemTime::now();
        eprintln!(
            "reboot: request at unix {:.1}, control dir {}",
            reboot_at
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64(),
            it.t.control_dir.display()
        );
        let begin = Instant::now();
        let trace_from = trace.lock().unwrap().len();
        handle.reboot_run().await.unwrap();

        // The first SSH answer with a lower uptime is the earliest moment after the boot.
        let uptime_after = loop {
            assert!(
                begin.elapsed() < REBOOT_WINDOW,
                "the switch did not come back within {REBOOT_WINDOW:?}"
            );
            if let Some(uptime) = uptime_read(t).await
                && uptime < uptime_before
            {
                break uptime;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        };
        let ssh_back_at = SystemTime::now();
        let save_log: Arc<Mutex<Vec<String>>> = Arc::default();
        let save_watch = save_state_watch_spawn(it.t.clone(), save_log.clone());
        let running = running_now(t).await;
        let repaired_before_read = handle
            .state_get()
            .await
            .overlay_last_run
            .is_some_and(|at| at >= reboot_at);
        eprintln!(
            "reboot: SSH back after {:.1} s, switch uptime {uptime_after:.1} s, \
repair already run: {repaired_before_read}",
            begin.elapsed().as_secs_f64()
        );
        let block = runcfg_port_block_get(&running, "te3");
        eprintln!("reboot: te3 at the first read {block:?}");
        let has = |line: &str| block.iter().any(|l| l == line);
        assert!(
            has(&format!(" description \"{name}\"")),
            "name after boot: {block:?}"
        );
        assert!(has(" spanning-tree edge"), "edge after boot: {block:?}");
        assert!(
            has(&format!(" switchport hybrid pvid {native_new}")),
            "native VLAN after boot: {block:?}"
        );
        assert!(has(" shutdown"), "port disable after boot: {block:?}");
        if !repaired_before_read {
            assert!(
                !has(" spanning-tree bpdu-filter"),
                "the filter is in the running config before the repair: {block:?}"
            );
        }
        if lock_take(t).await {
            *it.lock_taken.lock().unwrap() = true;
        }

        let took = state_wait(&handle, Duration::from_secs(120), "Reboot event", |s| {
            event_find(s, &EventKind::Reboot, reboot_at).is_some()
        })
        .await;
        let reboot_event = event_find(&handle.state_get().await, &EventKind::Reboot, reboot_at)
            .expect("Reboot event");
        eprintln!("reboot: event after {:.1} s more", took.as_secs_f64());
        assert!(
            reboot_event.requested,
            "the Reboot event is not marked requested"
        );

        // The overlay check and the poll run on their own timers, so the repair can come
        // before or after the Reboot event; both start when the switch answers SSH again.
        let limit = OVERLAY_CHECK + Duration::from_secs(10);
        let mut timeline: Vec<String> = Vec::new();
        let mut last_note = String::new();
        let mut repaired_at: Option<SystemTime> = None;
        loop {
            let since = SystemTime::now()
                .duration_since(ssh_back_at)
                .unwrap_or_default();
            let filter = port_block_now(t, "te3")
                .await
                .iter()
                .any(|l| l == " spanning-tree bpdu-filter");
            let state = handle.state_get().await;
            let kinds: Vec<String> = state
                .events
                .iter()
                .filter(|e| e.at >= reboot_at)
                .map(|e| format!("{:?}", e.kind))
                .collect();
            let note = format!(
                "filter {filter}; events {kinds:?}; last failure {:?}",
                state.last_failure
            );
            if note != last_note {
                timeline.push(format!("{:.1} s: {note}", since.as_secs_f64()));
                last_note = note;
            }
            if let Some(event) = event_find(&state, &EventKind::OverlayRepaired, reboot_at)
                && filter
            {
                repaired_at = Some(event.at);
                break;
            }
            if since > limit + Duration::from_secs(60) {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let repair_after = repaired_at.map(|at| at.duration_since(ssh_back_at).unwrap_or_default());
        if repair_after.is_none_or(|after| after > limit) {
            eprintln!("reboot: timeline after the SSH answer:");
            for line in &timeline {
                eprintln!("  {line}");
            }
            eprintln!("reboot: worker calls from the reboot request on:");
            for line in trace.lock().unwrap().iter().skip(trace_from) {
                eprintln!("  {line}");
            }
            panic!(
                "the overlay repair took {repair_after:?} after the SSH answer, limit {limit:?}"
            );
        }
        let repair_after = repair_after.unwrap_or_default();
        let order = if repaired_at.is_some_and(|at| at < reboot_event.at) {
            "before"
        } else {
            "after"
        };
        eprintln!(
            "reboot: OverlayRepaired {:.1} s after the SSH answer, {order} the Reboot event",
            repair_after.as_secs_f64()
        );

        eprintln!(
            "reboot: at the revert: {}",
            save_state_read(t).await.unwrap_or_default()
        );
        let result = handle
            .change_run(
                port_change(PortSettingsPatch {
                    name: Some(s0.name.clone()),
                    edge: Some(s0.edge),
                    native_vlan: Some(s0.native_vlan),
                    bpdu_filter: Some(s0.bpdu_filter),
                    enabled: Some(s0.enabled),
                    ..Default::default()
                }),
                false,
            )
            .await;
        save_watch.abort();
        eprintln!("reboot: save readiness from the SSH answer on:");
        for line in save_log.lock().unwrap().iter() {
            eprintln!("  {line}");
        }
        let result = result.unwrap();
        assert!(result.readback_ok, "readback_ok of the revert");
        start_state_check(&it, "reboot revert").await;
        load_back_wait(&handle, "reboot", &macs_start).await;
    })
    .await;
}
