// Integration tests for the change sequence, its read-back and the backup store.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use uswctl::backup::{BackupStore, backup_restore, stamp_format};
use uswctl::change::{
    Change, ChangeError, SwitchCtx, change_apply, change_effects_check, change_plan,
};
use uswctl::model::Model;
use uswctl::overlay::{Overlay, overlay_render};
use uswctl::settings::{
    BpduAction, EdgeMode, GlobalSettingsPatch, PoeMode, PortSettingsPatch, global_patch_apply,
    port_patch_apply,
};
use uswctl::syscfg::{syscfg_get, syscfg_parse, syscfg_render};
use uswctl::transport::{FakeTransport, Output, Transport};

const XG10_CFG: &str = include_str!("fixtures/xg10/system.cfg");
const XG10_RUNNING: &str = include_str!("fixtures/xg10/running-config.txt");
const XG10_POE: &str = include_str!("fixtures/xg10/poe-show.txt");
const PROMAX16_CFG: &str = include_str!("fixtures/promax16/system.cfg");
const PROMAX16_RUNNING: &str = include_str!("fixtures/promax16/running-config.txt");
const PROMAX16_POE: &str = include_str!("fixtures/promax16/poe-show.txt");

const TOKEN: &str = "ubiquity-gui";
const LOCK_PATH: &str = "/tmp/agent-lock";
const OVERLAY_PATH: &str = "/etc/persistent/uswctl-overlay.scr";
const NO_FILE: &str = "__USWCTL_NOFILE__\n";

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

// What the fake switch answers. `syscfg_after` is what `cat /tmp/system.cfg` returns from
// its second call on, that is after the put. `flash` is the flash read-back, and `None`
// means `syscfg_after`. An empty `lock` and a `None` overlay are missing files.
#[derive(Clone)]
struct FakeSwitch {
    lock: String,
    lock_status: i32,
    overlay_status: i32,
    flash: Option<String>,
    flash_status: i32,
    version: String,
    syscfg_before: String,
    syscfg_after: String,
    overlay: Option<String>,
    running_after: String,
    poe_after: String,
    apply_status: i32,
    // The number of the first apply calls that fail with exit status 1 before
    // `apply_status` holds.
    apply_failures: usize,
    save_status: i32,
    // The number of reads that find the management file empty, as early after a boot.
    mgmt_empty_reads: usize,
}

impl FakeSwitch {
    fn xg10() -> FakeSwitch {
        FakeSwitch {
            lock: String::new(),
            lock_status: 0,
            overlay_status: 0,
            flash: None,
            flash_status: 0,
            version: "7.5.15.17146\n".to_string(),
            syscfg_before: XG10_CFG.to_string(),
            syscfg_after: XG10_CFG.to_string(),
            overlay: None,
            running_after: XG10_RUNNING.to_string(),
            poe_after: XG10_POE.to_string(),
            apply_status: 0,
            apply_failures: 0,
            save_status: 0,
            mgmt_empty_reads: 0,
        }
    }
}

fn fake_switch_transport(sw: FakeSwitch) -> FakeTransport {
    let cfg_reads = Arc::new(AtomicUsize::new(0));
    let mgmt_reads = Arc::new(AtomicUsize::new(0));
    let apply_calls = Arc::new(AtomicUsize::new(0));
    FakeTransport::fake_new(move |script| {
        if script.contains("/etc/persistent/cfg/mgmt") {
            let n = mgmt_reads.fetch_add(1, Ordering::SeqCst);
            return out(
                0,
                if n < sw.mgmt_empty_reads {
                    "empty\n"
                } else {
                    "ready\n"
                },
            );
        }
        if script.contains("cfgmtd -r") {
            let text = sw.flash.as_ref().unwrap_or(&sw.syscfg_after);
            return out(sw.flash_status, text);
        }
        if script.contains("/tmp/agent-lock") {
            let text = if sw.lock.is_empty() {
                NO_FILE
            } else {
                &sw.lock
            };
            return out(sw.lock_status, text);
        }
        if script.contains("mca-dump") {
            return out(0, &sw.version);
        }
        if script.contains("cat /tmp/system.cfg") {
            let n = cfg_reads.fetch_add(1, Ordering::SeqCst);
            return out(
                0,
                if n == 0 {
                    &sw.syscfg_before
                } else {
                    &sw.syscfg_after
                },
            );
        }
        if script.contains(&format!("cat {OVERLAY_PATH}")) {
            return out(sw.overlay_status, sw.overlay.as_deref().unwrap_or(NO_FILE));
        }
        if script.contains("apply-config") {
            let n = apply_calls.fetch_add(1, Ordering::SeqCst);
            let status = if n < sw.apply_failures {
                1
            } else {
                sw.apply_status
            };
            return out(status, "");
        }
        if script.contains("save-config") {
            return out(sw.save_status, "");
        }
        if script.contains("show running-config") {
            return out(0, &sw.running_after);
        }
        if script.contains("swctrl poe show") {
            return out(0, &sw.poe_after);
        }
        if script.contains("cli -e") {
            return out(0, "");
        }
        out(127, "")
    })
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

fn syscfg_patched(text: &str, model: Model, idx: u8, patch: &PortSettingsPatch) -> String {
    let mut cfg = syscfg_parse(text);
    let mut overlay = Overlay::default();
    port_patch_apply(&mut cfg, &mut overlay, model, idx, patch).unwrap();
    syscfg_render(&cfg)
}

fn scripts_any(fake: &FakeTransport, needle: &str) -> bool {
    fake.scripts_get().iter().any(|s| s.contains(needle))
}

fn store_temp() -> (tempfile::TempDir, BackupStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = BackupStore {
        dir: dir.path().join("backups"),
    };
    (dir, store)
}

#[tokio::test]
async fn locked_by_other() {
    let mut sw = FakeSwitch::xg10();
    sw.lock = "other-session host-b 2026-11-03T10:00:00Z firmware\n".to_string();
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    let result = change_apply(&fake, &ctx_xg10(), &store, &name_change(3, "DUT 3")).await;
    match result {
        Err(ChangeError::Locked(text)) => assert!(text.contains("other-session")),
        other => panic!("expected Locked, got {other:?}"),
    }
    assert!(fake.file_puts_get().is_empty());
    assert!(!scripts_any(&fake, "apply-config"));
}

#[tokio::test]
async fn empty_token_matches_no_lock() {
    let mut sw = FakeSwitch::xg10();
    sw.lock = "other-session host-b".to_string();
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    let ctx = SwitchCtx {
        lock_token: String::new(),
        ..ctx_xg10()
    };
    let result = change_apply(&fake, &ctx, &store, &name_change(3, "DUT 3")).await;
    assert!(matches!(result, Err(ChangeError::Locked(_))), "{result:?}");
}

#[tokio::test]
async fn own_lock_allowed() {
    let patch = PortSettingsPatch {
        name: Some("DUT 3".to_string()),
        ..Default::default()
    };
    let mut sw = FakeSwitch::xg10();
    sw.lock = format!("{TOKEN} host-b 2026-11-03T10:00:00Z test\n");
    sw.syscfg_after = syscfg_patched(XG10_CFG, Model::Xg10, 3, &patch);
    sw.running_after = XG10_RUNNING.replace("description \"Port 3\"", "description \"DUT 3\"");
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    let result = change_apply(&fake, &ctx_xg10(), &store, &Change::Port { idx: 3, patch })
        .await
        .expect("own lock must allow the change");
    assert!(result.readback_ok);
}

// Without a lock path the daemon reads no lock file, so even a foreign lock lets the
// change through.
#[tokio::test]
async fn no_lock_path_ignores_lock() {
    let patch = PortSettingsPatch {
        name: Some("DUT 3".to_string()),
        ..Default::default()
    };
    let mut sw = FakeSwitch::xg10();
    sw.lock = "other-session host-b 2026-11-03T10:00:00Z firmware\n".to_string();
    sw.syscfg_after = syscfg_patched(XG10_CFG, Model::Xg10, 3, &patch);
    sw.running_after = XG10_RUNNING.replace("description \"Port 3\"", "description \"DUT 3\"");
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    let ctx = SwitchCtx {
        lock_path: None,
        ..ctx_xg10()
    };
    let result = change_apply(&fake, &ctx, &store, &Change::Port { idx: 3, patch })
        .await
        .expect("no lock path must allow the change");
    assert!(result.readback_ok);
    let scripts = fake.scripts_get();
    assert!(
        !scripts.iter().any(|s| s.contains(LOCK_PATH)),
        "{scripts:?}"
    );
}

#[tokio::test]
async fn whitespace_lock_is_no_lock() {
    let patch = PortSettingsPatch {
        name: Some("DUT 3".to_string()),
        ..Default::default()
    };
    let mut sw = FakeSwitch::xg10();
    sw.lock = " \n".to_string();
    sw.syscfg_after = syscfg_patched(XG10_CFG, Model::Xg10, 3, &patch);
    sw.running_after = XG10_RUNNING.replace("description \"Port 3\"", "description \"DUT 3\"");
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    change_apply(&fake, &ctx_xg10(), &store, &Change::Port { idx: 3, patch })
        .await
        .expect("a whitespace lock file is no lock");
}

#[tokio::test]
async fn untested_firmware_refused() {
    let mut sw = FakeSwitch::xg10();
    sw.version = "7.6.1.18000\n".to_string();
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    let result = change_apply(&fake, &ctx_xg10(), &store, &name_change(3, "DUT 3")).await;
    match result {
        Err(ChangeError::UntestedFirmware(version)) => assert_eq!(version, "7.6.1.18000"),
        other => panic!("expected UntestedFirmware, got {other:?}"),
    }
    assert!(fake.file_puts_get().is_empty());
}

#[tokio::test]
async fn apply_failure_skips_save() {
    let mut sw = FakeSwitch::xg10();
    sw.apply_status = 1;
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    let result = change_apply(&fake, &ctx_xg10(), &store, &name_change(3, "DUT 3")).await;
    match &result {
        Err(ChangeError::ApplyFailed(text)) => {
            assert!(text.contains("rollback failed: system.cfg"), "{text}")
        }
        other => panic!("expected ApplyFailed, got {other:?}"),
    }
    assert!(!scripts_any(&fake, "save-config"));
    assert!(store.backup_list("xg10").is_empty());
    assert_eq!(syscfg_put_get(&fake), XG10_CFG, "the old text is put back");
}

fn syscfg_put_get(fake: &FakeTransport) -> String {
    String::from_utf8(fake.files_get()["/tmp/system.cfg"].clone()).unwrap()
}

fn puts_count(fake: &FakeTransport, path: &str) -> usize {
    fake.file_puts_get().iter().filter(|p| *p == path).count()
}

fn scripts_count(fake: &FakeTransport, needle: &str) -> usize {
    fake.scripts_get()
        .iter()
        .filter(|s| s.contains(needle))
        .count()
}

#[tokio::test]
async fn apply_failure_rolls_back() {
    let mut sw = FakeSwitch::xg10();
    sw.apply_failures = 1;
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    let result = change_apply(&fake, &ctx_xg10(), &store, &name_change(3, "DUT 3")).await;
    match &result {
        Err(ChangeError::ApplyFailed(text)) => {
            assert!(text.ends_with("; rollback: restored"), "{text}")
        }
        other => panic!("expected ApplyFailed, got {other:?}"),
    }
    assert_eq!(puts_count(&fake, "/tmp/system.cfg"), 2);
    assert_eq!(syscfg_put_get(&fake), XG10_CFG, "the old text is put back");
    assert_eq!(scripts_count(&fake, "apply-config"), 2);
    assert!(!scripts_any(&fake, "save-config"));
    assert!(fake.file_puts_get().iter().all(|p| p != OVERLAY_PATH));
}

#[tokio::test]
async fn save_failure_rolls_back() {
    let mut sw = FakeSwitch::xg10();
    sw.save_status = 1;
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    let result = change_apply(&fake, &ctx_xg10(), &store, &name_change(3, "DUT 3")).await;
    match &result {
        Err(ChangeError::SaveFailed(text)) => {
            assert!(text.ends_with("; rollback: restored"), "{text}")
        }
        other => panic!("expected SaveFailed, got {other:?}"),
    }
    assert_eq!(syscfg_put_get(&fake), XG10_CFG, "the old text is put back");
    let scripts = fake.scripts_get();
    let save = scripts
        .iter()
        .position(|s| s.contains("save-config"))
        .unwrap();
    assert!(
        scripts[save + 1..]
            .iter()
            .any(|s| s == "#file_put /tmp/system.cfg")
    );
    assert!(
        scripts[save + 1..]
            .iter()
            .any(|s| s.contains("apply-config"))
    );
    assert_eq!(
        scripts_count(&fake, "save-config"),
        1,
        "no save of the rollback"
    );
    assert!(store.backup_list("xg10").is_empty());
}

#[tokio::test]
async fn save_failure_reported() {
    let patch = PortSettingsPatch {
        name: Some("DUT 3".to_string()),
        ..Default::default()
    };
    let mut sw = FakeSwitch::xg10();
    sw.save_status = 1;
    sw.syscfg_after = syscfg_patched(XG10_CFG, Model::Xg10, 3, &patch);
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    let result = change_apply(&fake, &ctx_xg10(), &store, &Change::Port { idx: 3, patch }).await;
    assert!(
        matches!(result, Err(ChangeError::SaveFailed(_))),
        "expected SaveFailed, got {result:?}"
    );
    assert!(!scripts_any(&fake, "show running-config"));
}

#[tokio::test]
async fn no_effect_reported() {
    let patch = PortSettingsPatch {
        name: Some("DUT 3".to_string()),
        ..Default::default()
    };
    let mut sw = FakeSwitch::xg10();
    sw.syscfg_after = syscfg_patched(XG10_CFG, Model::Xg10, 3, &patch);
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    let result = change_apply(&fake, &ctx_xg10(), &store, &Change::Port { idx: 3, patch }).await;
    match result {
        Err(ChangeError::NoEffect(missing)) => {
            assert_eq!(missing.len(), 1, "{missing:?}");
            assert!(missing[0].contains("port 3"), "{missing:?}");
            assert!(missing[0].contains("name"), "{missing:?}");
        }
        other => panic!("expected NoEffect, got {other:?}"),
    }
    assert!(
        scripts_any(&fake, "save-config"),
        "NoEffect comes after the save"
    );
    assert!(store.backup_list("xg10").is_empty());
}

#[tokio::test]
async fn no_effect_in_syscfg_readback() {
    let patch = PortSettingsPatch {
        name: Some("DUT 3".to_string()),
        ..Default::default()
    };
    let mut sw = FakeSwitch::xg10();
    sw.flash = Some(syscfg_patched(XG10_CFG, Model::Xg10, 3, &patch));
    sw.running_after = XG10_RUNNING.replace("description \"Port 3\"", "description \"DUT 3\"");
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    let result = change_apply(&fake, &ctx_xg10(), &store, &Change::Port { idx: 3, patch }).await;
    match result {
        Err(ChangeError::NoEffect(missing)) => {
            assert_eq!(
                missing,
                vec!["/tmp/system.cfg switch.port.3.name".to_string()]
            );
        }
        other => panic!("expected NoEffect, got {other:?}"),
    }
}

#[tokio::test]
async fn happy_path_order() {
    let patch = PortSettingsPatch {
        name: Some("DUT+3".to_string()),
        poe: Some(PoeMode::Off),
        ..Default::default()
    };
    let mut sw = FakeSwitch::xg10();
    sw.syscfg_after = syscfg_patched(XG10_CFG, Model::Xg10, 3, &patch);
    sw.running_after = XG10_RUNNING.replace("description \"Port 3\"", "description \"DUT_3\"");
    sw.poe_after = XG10_POE.replace(
        "   3    Auto  Dot3bt-type4     96000",
        "   3     Off  Dot3bt-type4     96000",
    );
    let syscfg_after = sw.syscfg_after.clone();
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    let result = change_apply(&fake, &ctx_xg10(), &store, &Change::Port { idx: 3, patch })
        .await
        .expect("happy path");

    assert!(result.readback_ok);
    assert!(!result.overlay_changed);
    assert!(result.cli_lines.is_empty());
    let keys: Vec<&str> = result.keys.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(keys, vec!["switch.port.3.name", "switch.port.3.poe"]);

    let scripts = fake.scripts_get();
    let position = |needle: &str| {
        scripts
            .iter()
            .position(|s| s.contains(needle))
            .unwrap_or_else(|| panic!("no script with {needle:?} in {scripts:#?}"))
    };
    let lock = position("/tmp/agent-lock");
    let version = position("mca-dump | jq -r .version");
    let read = position("cat /tmp/system.cfg");
    let put = position("#file_put /tmp/system.cfg");
    let apply = position("syswrapper.sh apply-config");
    let save = position("syswrapper.sh save-config && syswrapper.sh cfg_save_check");
    let running = position("cli -c \"terminal length 0\" -c \"show running-config\"");
    let poe = position("swctrl poe show id 3");
    let flash = position("cfgmtd -r -p /tmp/uswctl-read -f /tmp/uswctl-read/read.cfg");
    assert!(lock < version && version < read && read < put && put < apply);
    assert!(apply < save && save < flash && save < running && save < poe);
    assert!(scripts[flash].contains("rm -rf /tmp/uswctl-read; exit $rc"));
    assert!(
        scripts[save + 1..]
            .iter()
            .any(|s| s.contains("cat /tmp/system.cfg"))
    );
    assert!(!scripts_any(&fake, "cli -e"));
    assert!(!fake.file_puts_get().iter().any(|p| p == OVERLAY_PATH));

    let put_text = fake.files_get().get("/tmp/system.cfg").cloned().unwrap();
    assert_eq!(String::from_utf8(put_text).unwrap(), syscfg_after);

    let stamps = store.backup_list("xg10");
    assert_eq!(stamps.len(), 1);
    let dir = store.dir.join("xg10").join(&stamps[0]);
    let saved_cfg = std::fs::read_to_string(dir.join("system.cfg")).unwrap();
    assert_eq!(saved_cfg, syscfg_after);
    let saved_overlay = std::fs::read_to_string(dir.join("uswctl-overlay.scr")).unwrap();
    assert_eq!(
        saved_overlay,
        overlay_render(&Overlay::default(), Model::Xg10)
    );
    for name in ["system.cfg", "uswctl-overlay.scr"] {
        let mode = std::fs::metadata(dir.join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{name}");
    }
}

#[tokio::test]
async fn overlay_change_runs_cli_and_checks_it() {
    let patch = PortSettingsPatch {
        bpdu_filter: Some(true),
        ..Default::default()
    };
    let mut sw = FakeSwitch::xg10();
    sw.running_after = XG10_RUNNING.replace(
        "interface te3\n switchport mode hybrid\n",
        "interface te3\n spanning-tree bpdu-filter\n switchport mode hybrid\n",
    );
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    let result = change_apply(&fake, &ctx_xg10(), &store, &Change::Port { idx: 3, patch })
        .await
        .expect("overlay change");
    assert!(result.keys.is_empty());
    assert!(result.overlay_changed);
    assert_eq!(
        result.cli_lines,
        vec![" spanning-tree bpdu-filter".to_string()]
    );
    assert!(
        !scripts_any(&fake, "apply-config"),
        "no system.cfg change, no apply"
    );
    let scripts = fake.scripts_get();
    let put = scripts
        .iter()
        .position(|s| s == &format!("#file_put {OVERLAY_PATH}"))
        .unwrap();
    let run = scripts
        .iter()
        .position(|s| s.contains(&format!("cli -e {OVERLAY_PATH}")))
        .unwrap();
    let save = scripts
        .iter()
        .position(|s| s.contains("save-config"))
        .unwrap();
    assert!(put < run && run < save);
    let mut expected = Overlay::default();
    expected.port_bpdu_filter.insert(3);
    let put_text = fake.files_get().get(OVERLAY_PATH).cloned().unwrap();
    assert_eq!(
        String::from_utf8(put_text).unwrap(),
        overlay_render(&expected, Model::Xg10)
    );
}

#[tokio::test]
async fn overlay_no_effect_reported() {
    let patch = PortSettingsPatch {
        bpdu_filter: Some(true),
        ..Default::default()
    };
    let fake = fake_switch_transport(FakeSwitch::xg10());
    let (_dir, store) = store_temp();
    let result = change_apply(&fake, &ctx_xg10(), &store, &Change::Port { idx: 3, patch }).await;
    match result {
        Err(ChangeError::NoEffect(missing)) => {
            assert!(
                missing.iter().any(|m| m.contains("bpdu_filter")),
                "{missing:?}"
            );
        }
        other => panic!("expected NoEffect, got {other:?}"),
    }
}

#[tokio::test]
async fn unchanged_request_writes_nothing() {
    let fake = fake_switch_transport(FakeSwitch::xg10());
    let (_dir, store) = store_temp();
    let result = change_apply(&fake, &ctx_xg10(), &store, &name_change(3, "Port 3"))
        .await
        .unwrap();
    assert!(result.keys.is_empty());
    assert!(!result.overlay_changed);
    assert!(fake.file_puts_get().is_empty());
    assert!(!scripts_any(&fake, "save-config"));
}

struct SaveUnreachable {
    inner: FakeTransport,
}

#[async_trait]
impl Transport for SaveUnreachable {
    async fn script_run(&self, script: &str) -> anyhow::Result<Output> {
        if script.contains("save-config") {
            anyhow::bail!("ssh connection failed (exit 255): timeout");
        }
        self.inner.script_run(script).await
    }

    async fn file_put(&self, path: &str, data: &[u8]) -> anyhow::Result<()> {
        self.inner.file_put(path, data).await
    }
}

#[tokio::test]
async fn unreachable_mid_change() {
    let t = SaveUnreachable {
        inner: fake_switch_transport(FakeSwitch::xg10()),
    };
    let (_dir, store) = store_temp();
    let result = change_apply(&t, &ctx_xg10(), &store, &name_change(3, "DUT 3")).await;
    match result {
        Err(ChangeError::Unreachable(message)) => {
            assert!(message.contains("exit 255"), "{message}");
            assert!(message.ends_with("; rollback: restored"), "{message}");
        }
        other => panic!("expected Unreachable, got {other:?}"),
    }
    assert!(
        !t.inner
            .scripts_get()
            .iter()
            .any(|s| s.contains("show running-config"))
    );
    assert_eq!(
        syscfg_put_get(&t.inner),
        XG10_CFG,
        "the old text is put back"
    );
}

// The first script that contains `needle` fails as a lost SSH connection; every other
// call reaches `inner`.
struct FailOnce {
    inner: FakeTransport,
    needle: &'static str,
    failed: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl Transport for FailOnce {
    async fn script_run(&self, script: &str) -> anyhow::Result<Output> {
        if script.contains(self.needle) && !self.failed.swap(true, Ordering::SeqCst) {
            anyhow::bail!("ssh connection failed (exit 255): timeout");
        }
        self.inner.script_run(script).await
    }

    async fn file_put(&self, path: &str, data: &[u8]) -> anyhow::Result<()> {
        self.inner.file_put(path, data).await
    }
}

fn fail_once_build(sw: FakeSwitch, needle: &'static str) -> FailOnce {
    FailOnce {
        inner: fake_switch_transport(sw),
        needle,
        failed: std::sync::atomic::AtomicBool::new(false),
    }
}

#[tokio::test]
async fn unreachable_after_put_rolls_back() {
    let t = fail_once_build(FakeSwitch::xg10(), "apply-config");
    let (_dir, store) = store_temp();
    let result = change_apply(&t, &ctx_xg10(), &store, &name_change(3, "DUT 3")).await;
    match &result {
        Err(ChangeError::Unreachable(text)) => {
            assert!(text.ends_with("; rollback: restored"), "{text}")
        }
        other => panic!("expected Unreachable, got {other:?}"),
    }
    assert_eq!(puts_count(&t.inner, "/tmp/system.cfg"), 2);
    assert_eq!(
        syscfg_put_get(&t.inner),
        XG10_CFG,
        "the old text is put back"
    );
    assert!(!scripts_any(&t.inner, "save-config"));
}

#[tokio::test]
async fn overlay_failure_rolls_back_overlay() {
    let t = fail_once_build(FakeSwitch::xg10(), "cli -e");
    let (_dir, store) = store_temp();
    let change = Change::Port {
        idx: 3,
        patch: PortSettingsPatch {
            bpdu_filter: Some(true),
            ..Default::default()
        },
    };
    let result = change_apply(&t, &ctx_xg10(), &store, &change).await;
    match &result {
        Err(ChangeError::Unreachable(text)) => {
            assert!(text.ends_with("; rollback: restored"), "{text}")
        }
        other => panic!("expected Unreachable, got {other:?}"),
    }
    let put = t.inner.files_get()[OVERLAY_PATH].clone();
    assert_eq!(
        String::from_utf8(put).unwrap(),
        overlay_render(&Overlay::default(), Model::Xg10),
        "the old overlay is put back"
    );
    assert_eq!(puts_count(&t.inner, OVERLAY_PATH), 2);
    // The failed run does not reach the record of the fake; the rollback run does.
    assert_eq!(
        scripts_count(&t.inner, &format!("cli -e {OVERLAY_PATH}")),
        1
    );
    assert_eq!(puts_count(&t.inner, "/tmp/system.cfg"), 0);
}

#[tokio::test]
async fn restore_save_failure_rolls_back() {
    let (_dir, store) = store_temp();
    let backup_cfg = XG10_CFG.replace("switch.port.3.name=Port 3", "switch.port.3.name=Saved 3");
    let stamp = backup_filtering(&store, &backup_cfg);
    let mut sw = FakeSwitch::xg10();
    sw.save_status = 1;
    let fake = fake_switch_transport(sw);
    let result = backup_restore(&fake, &ctx_xg10(), &store, &stamp).await;
    match &result {
        Err(ChangeError::SaveFailed(text)) => {
            assert!(text.ends_with("; rollback: restored"), "{text}")
        }
        other => panic!("expected SaveFailed, got {other:?}"),
    }
    assert_eq!(syscfg_put_get(&fake), XG10_CFG, "the old text is put back");
    let overlay = fake.files_get()[OVERLAY_PATH].clone();
    assert_eq!(
        String::from_utf8(overlay).unwrap(),
        overlay_render(&Overlay::default(), Model::Xg10),
        "the old overlay is put back"
    );
    assert_eq!(store.backup_list("xg10").len(), 1, "no new backup");
}

#[tokio::test]
async fn plan_does_not_write() {
    let mut sw = FakeSwitch::xg10();
    sw.lock = "other-session host-b 2026-11-03T10:00:00Z firmware\n".to_string();
    let fake = fake_switch_transport(sw);
    let change = Change::Port {
        idx: 3,
        patch: PortSettingsPatch {
            name: Some("DUT 3".to_string()),
            bpdu_filter: Some(true),
            ..Default::default()
        },
    };
    let result = change_plan(&fake, &ctx_xg10(), &change).await.unwrap();
    assert!(!result.readback_ok);
    assert!(result.overlay_changed);
    assert_eq!(
        result.cli_lines,
        vec![" spanning-tree bpdu-filter".to_string()]
    );
    assert_eq!(result.keys.len(), 1);
    assert_eq!(result.keys[0].key, "switch.port.3.name");
    assert_eq!(result.keys[0].new.as_deref(), Some("DUT 3"));
    assert!(fake.file_puts_get().is_empty());
    let scripts = fake.scripts_get();
    assert!(!scripts.iter().any(|s| s.contains("agent-lock")));
    for needle in [
        "apply-config",
        "save-config",
        "cli -e",
        "show running-config",
    ] {
        assert!(!scripts.iter().any(|s| s.contains(needle)), "{needle}");
    }
}

#[tokio::test]
async fn plan_refuses_untested_firmware() {
    let mut sw = FakeSwitch::xg10();
    sw.version = String::new();
    let fake = fake_switch_transport(sw);
    let result = change_plan(&fake, &ctx_xg10(), &name_change(3, "DUT 3")).await;
    assert!(
        matches!(result, Err(ChangeError::UntestedFirmware(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn plan_reports_settings_error() {
    let fake = fake_switch_transport(FakeSwitch::xg10());
    let result = change_plan(&fake, &ctx_xg10(), &name_change(13, "X")).await;
    assert!(
        matches!(result, Err(ChangeError::Settings(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn restore_writes_backup_files() {
    let (_dir, store) = store_temp();
    let backup_cfg = XG10_CFG.replace("switch.port.3.name=Port 3", "switch.port.3.name=Saved 3");
    let overlay = Overlay {
        bpdu_action: BpduAction::Filtering,
        ..Default::default()
    };
    let backup_overlay = overlay_render(&overlay, Model::Xg10);
    store
        .backup_save("xg10", &backup_cfg, &backup_overlay)
        .unwrap();
    let stamp = store.backup_list("xg10").remove(0);

    let mut sw = FakeSwitch::xg10();
    sw.syscfg_after = backup_cfg.clone();
    sw.running_after = running_filtering();
    let fake = fake_switch_transport(sw);
    let result = backup_restore(&fake, &ctx_xg10(), &store, &stamp)
        .await
        .expect("restore");

    assert!(result.readback_ok);
    assert!(result.overlay_changed);
    let keys: Vec<&str> = result.keys.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(keys, vec!["switch.port.3.name"]);
    let files = fake.files_get();
    assert_eq!(
        String::from_utf8(files["/tmp/system.cfg"].clone()).unwrap(),
        backup_cfg
    );
    assert_eq!(
        String::from_utf8(files[OVERLAY_PATH].clone()).unwrap(),
        backup_overlay
    );
    let scripts = fake.scripts_get();
    let position = |needle: &str| scripts.iter().position(|s| s.contains(needle)).unwrap();
    assert!(position("agent-lock") < position("mca-dump"));
    assert!(position("mca-dump") < position("#file_put /tmp/system.cfg"));
    assert!(position("apply-config") < position(&format!("cli -e {OVERLAY_PATH}")));
    assert!(position(&format!("cli -e {OVERLAY_PATH}")) < position("save-config"));
    assert!(position("save-config") < position("cfgmtd -r"));
    assert!(position("save-config") < position("show running-config"));
}

fn running_filtering() -> String {
    XG10_RUNNING.replace(
        "storm-control unit level\n",
        "storm-control unit level\nspanning-tree bpdu filtering\n",
    )
}

fn backup_filtering(store: &BackupStore, cfg: &str) -> String {
    let overlay = Overlay {
        bpdu_action: BpduAction::Filtering,
        ..Default::default()
    };
    store
        .backup_save("xg10", cfg, &overlay_render(&overlay, Model::Xg10))
        .unwrap();
    store.backup_list("xg10").remove(0)
}

#[tokio::test]
async fn restore_reports_overlay_not_shown() {
    let (_dir, store) = store_temp();
    let stamp = backup_filtering(&store, XG10_CFG);
    let fake = fake_switch_transport(FakeSwitch::xg10());
    let result = backup_restore(&fake, &ctx_xg10(), &store, &stamp).await;
    match result {
        Err(ChangeError::NoEffect(missing)) => {
            assert_eq!(missing, vec![format!("overlay of backup {stamp}")]);
        }
        other => panic!("expected NoEffect, got {other:?}"),
    }
}

#[tokio::test]
async fn restore_reports_flash_mismatch() {
    let (_dir, store) = store_temp();
    let stamp = backup_filtering(&store, XG10_CFG);
    let mut sw = FakeSwitch::xg10();
    sw.running_after = running_filtering();
    sw.flash = Some(XG10_CFG.replace("switch.port.3.name=Port 3", "switch.port.3.name=Old"));
    let fake = fake_switch_transport(sw);
    let result = backup_restore(&fake, &ctx_xg10(), &store, &stamp).await;
    match result {
        Err(ChangeError::NoEffect(missing)) => {
            assert_eq!(missing, vec![format!("flash differs from backup {stamp}")]);
        }
        other => panic!("expected NoEffect, got {other:?}"),
    }
}

#[tokio::test]
async fn restore_flash_ignores_trailing_newline() {
    let (_dir, store) = store_temp();
    let stamp = backup_filtering(&store, XG10_CFG);
    let mut sw = FakeSwitch::xg10();
    sw.running_after = running_filtering();
    sw.flash = Some(format!("{XG10_CFG}\n"));
    let fake = fake_switch_transport(sw);
    backup_restore(&fake, &ctx_xg10(), &store, &stamp)
        .await
        .expect("a trailing newline is no difference");
}

#[tokio::test]
async fn flash_missing_key_reported() {
    let patch = PortSettingsPatch {
        name: Some("DUT 3".to_string()),
        ..Default::default()
    };
    let mut sw = FakeSwitch::xg10();
    sw.syscfg_after = syscfg_patched(XG10_CFG, Model::Xg10, 3, &patch);
    sw.flash = Some(XG10_CFG.to_string());
    sw.running_after = XG10_RUNNING.replace("description \"Port 3\"", "description \"DUT 3\"");
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    let result = change_apply(&fake, &ctx_xg10(), &store, &Change::Port { idx: 3, patch }).await;
    match result {
        Err(ChangeError::NoEffect(missing)) => {
            assert_eq!(missing, vec!["flash switch.port.3.name".to_string()]);
        }
        other => panic!("expected NoEffect, got {other:?}"),
    }
    assert!(store.backup_list("xg10").is_empty());
}

#[tokio::test]
async fn flash_removed_key_must_be_absent() {
    let patch = PortSettingsPatch {
        enabled: Some(true),
        ..Default::default()
    };
    let disabled = XG10_CFG.replace(
        "switch.port.3.name=Port 3\n",
        "switch.port.3.name=Port 3\nswitch.port.3.status=disabled\n",
    );
    let mut sw = FakeSwitch::xg10();
    sw.syscfg_before = disabled.clone();
    sw.syscfg_after = XG10_CFG.to_string();
    sw.flash = Some(disabled);
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    let result = change_apply(&fake, &ctx_xg10(), &store, &Change::Port { idx: 3, patch }).await;
    match result {
        Err(ChangeError::NoEffect(missing)) => {
            assert_eq!(missing, vec!["flash switch.port.3.status".to_string()]);
        }
        other => panic!("expected NoEffect, got {other:?}"),
    }
}

#[tokio::test]
async fn flash_read_failure_reported() {
    let mut sw = FakeSwitch::xg10();
    sw.flash_status = 1;
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    let result = change_apply(&fake, &ctx_xg10(), &store, &name_change(3, "DUT 3")).await;
    assert!(
        matches!(result, Err(ChangeError::ReadFailed(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn lock_read_failure_reported() {
    let mut sw = FakeSwitch::xg10();
    sw.lock_status = 1;
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    let result = change_apply(&fake, &ctx_xg10(), &store, &name_change(3, "DUT 3")).await;
    assert!(
        matches!(result, Err(ChangeError::ReadFailed(_))),
        "{result:?}"
    );
    assert!(fake.file_puts_get().is_empty());
}

#[tokio::test]
async fn overlay_read_failure_reported() {
    let mut sw = FakeSwitch::xg10();
    sw.overlay_status = 1;
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    let result = change_apply(&fake, &ctx_xg10(), &store, &name_change(3, "DUT 3")).await;
    assert!(
        matches!(result, Err(ChangeError::ReadFailed(_))),
        "{result:?}"
    );
    assert!(fake.file_puts_get().is_empty());
}

#[test]
fn backup_save_same_second_keeps_both() {
    let (_dir, store) = store_temp();
    let first = store.backup_save("sw", "a=1\n", "first\n").unwrap();
    let second = store.backup_save("sw", "a=2\n", "second\n").unwrap();
    assert_ne!(first, second);
    assert_eq!(store.backup_list("sw").len(), 2);
    let read = |dir: &std::path::Path, name: &str| std::fs::read_to_string(dir.join(name)).unwrap();
    assert_eq!(read(&first, "system.cfg"), "a=1\n");
    assert_eq!(read(&first, "uswctl-overlay.scr"), "first\n");
    assert_eq!(read(&second, "system.cfg"), "a=2\n");
    assert_eq!(read(&second, "uswctl-overlay.scr"), "second\n");
}

#[test]
fn effects_refuse_vacuous_output() {
    let cfg = syscfg_parse(XG10_CFG);
    let overlay = Overlay::default();
    let defaults = Change::Port {
        idx: 2,
        patch: PortSettingsPatch {
            edge: Some(EdgeMode::Auto),
            bpdu_guard: Some(false),
            enabled: Some(true),
            ..Default::default()
        },
    };
    assert_eq!(
        change_effects_check(Model::Xg10, &defaults, &cfg, &overlay, "", None),
        vec!["port 2: block not shown".to_string()]
    );
    let global = Change::Global {
        patch: GlobalSettingsPatch {
            stp_version: Some("rstp".to_string()),
            stp_priority: Some(32768),
            ..Default::default()
        },
    };
    assert_eq!(
        change_effects_check(Model::Xg10, &global, &cfg, &overlay, "", None),
        vec!["global: running config incomplete".to_string()]
    );
    let cut = XG10_RUNNING.split("interface te12").next().unwrap();
    assert_eq!(
        change_effects_check(Model::Xg10, &global, &cfg, &overlay, cut, None),
        vec!["global: running config incomplete".to_string()]
    );
    let filter = Change::Port {
        idx: 2,
        patch: PortSettingsPatch {
            bpdu_filter: Some(false),
            ..Default::default()
        },
    };
    assert_eq!(
        change_effects_check(Model::Xg10, &filter, &cfg, &overlay, cut, None),
        vec!["port 2: running config incomplete".to_string()]
    );
    let no_vlan_line = XG10_RUNNING.replace("vlan 2-8\n", "");
    assert_eq!(
        change_effects_check(Model::Xg10, &global, &cfg, &overlay, &no_vlan_line, None),
        vec!["global: running config incomplete".to_string()]
    );
}

#[tokio::test]
async fn restore_reports_mismatch() {
    let (_dir, store) = store_temp();
    let backup_cfg = XG10_CFG.replace("switch.port.3.name=Port 3", "switch.port.3.name=Saved 3");
    store.backup_save("xg10", &backup_cfg, "").unwrap();
    let stamp = store.backup_list("xg10").remove(0);
    let fake = fake_switch_transport(FakeSwitch::xg10());
    let result = backup_restore(&fake, &ctx_xg10(), &store, &stamp).await;
    assert!(
        matches!(result, Err(ChangeError::NoEffect(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn restore_refused_by_lock() {
    let (_dir, store) = store_temp();
    store.backup_save("xg10", XG10_CFG, "").unwrap();
    let stamp = store.backup_list("xg10").remove(0);
    let mut sw = FakeSwitch::xg10();
    sw.lock = "other-session host-b".to_string();
    let fake = fake_switch_transport(sw);
    let result = backup_restore(&fake, &ctx_xg10(), &store, &stamp).await;
    assert!(matches!(result, Err(ChangeError::Locked(_))), "{result:?}");
    assert!(fake.file_puts_get().is_empty());
}

#[tokio::test]
async fn restore_rejects_bad_stamp() {
    let (_dir, store) = store_temp();
    let fake = fake_switch_transport(FakeSwitch::xg10());
    for stamp in ["../../etc", "20261103T132545Z"] {
        let result = backup_restore(&fake, &ctx_xg10(), &store, stamp).await;
        assert!(result.is_err(), "{stamp}");
    }
    assert!(fake.scripts_get().is_empty());
}

#[test]
fn backup_list_newest_first_and_modes() {
    let (_dir, store) = store_temp();
    let base = store.dir.join("sw");
    for stamp in [
        "20261101T000000Z",
        "20261103T120000Z",
        "20261102T235959Z",
        "junk",
    ] {
        std::fs::create_dir_all(base.join(stamp)).unwrap();
    }
    assert_eq!(
        store.backup_list("sw"),
        vec!["20261103T120000Z", "20261102T235959Z", "20261101T000000Z"]
    );
    assert!(store.backup_list("absent").is_empty());

    let (_dir2, store2) = store_temp();
    let path = store2
        .backup_save("sw", "a=1\n", "no spanning-tree bpdu\n")
        .unwrap();
    for dir in [store2.dir.join("sw"), path.clone()] {
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", dir.display());
    }
    assert_eq!(
        std::fs::read_to_string(path.join("system.cfg")).unwrap(),
        "a=1\n"
    );
}

#[test]
fn backup_save_rejects_bad_switch_name() {
    let (_dir, store) = store_temp();
    for name in ["", "..", "a/b", "."] {
        assert!(store.backup_save(name, "", "").is_err(), "{name:?}");
    }
}

#[test]
fn stamp_format_values() {
    assert_eq!(stamp_format(0), "19700101T000000Z");
    assert_eq!(stamp_format(951_782_400), "20000229T000000Z");
    assert_eq!(stamp_format(1_793_712_345), "20261103T132545Z");
    assert_eq!(stamp_format(4_107_542_399), "21000228T235959Z");
}

// The effect checks against the captured state of both switches: each setting that a
// fixture shows must pass when the request asks for the value it already has.
fn effects_on_fixture(
    model: Model,
    cfg_text: &str,
    running: &str,
    poe: &str,
    change: &Change,
) -> Vec<String> {
    let mut cfg = syscfg_parse(cfg_text);
    let mut overlay = Overlay::default();
    match change {
        Change::Port { idx, patch } => {
            port_patch_apply(&mut cfg, &mut overlay, model, *idx, patch).unwrap()
        }
        Change::Global { patch } => global_patch_apply(&mut cfg, &mut overlay, patch).unwrap(),
    }
    change_effects_check(model, change, &cfg, &overlay, running, Some(poe))
}

fn port_change(idx: u8, patch: PortSettingsPatch) -> Change {
    Change::Port { idx, patch }
}

#[test]
fn effects_match_fixture_forms() {
    let pm = |idx, patch| {
        effects_on_fixture(
            Model::ProMax16,
            PROMAX16_CFG,
            PROMAX16_RUNNING,
            PROMAX16_POE,
            &port_change(idx, patch),
        )
    };
    let xg = |idx, patch| {
        effects_on_fixture(
            Model::Xg10,
            XG10_CFG,
            XG10_RUNNING,
            XG10_POE,
            &port_change(idx, patch),
        )
    };
    let p = PortSettingsPatch::default;

    // STP off, native VLAN 7, PoE Auto.
    assert!(
        pm(
            1,
            PortSettingsPatch {
                stp: Some(false),
                native_vlan: Some(Some(7)),
                tagged_vlans: Some(vec![]),
                poe: Some(PoeMode::Auto),
                ..p()
            }
        )
        .is_empty()
    );
    // Native VLAN 1 untagged, tagged 2-4, edge disabled, BPDU guard off, PoE off.
    assert!(
        pm(
            8,
            PortSettingsPatch {
                native_vlan: Some(Some(1)),
                tagged_vlans: Some(vec![2, 3, 4]),
                edge: Some(EdgeMode::Disabled),
                bpdu_guard: Some(false),
                poe: Some(PoeMode::Off),
                stp: Some(true),
                ..p()
            }
        )
        .is_empty()
    );
    // Edge enabled and BPDU guard on.
    assert!(
        pm(
            9,
            PortSettingsPatch {
                edge: Some(EdgeMode::Enabled),
                bpdu_guard: Some(true),
                ..p()
            }
        )
        .is_empty()
    );
    // '+' in a name shows as '_'.
    assert!(
        pm(
            17,
            PortSettingsPatch {
                name: Some("SFP+ 1".to_string()),
                ..p()
            }
        )
        .is_empty()
    );
    // Edge auto shows no edge line.
    assert!(
        xg(
            2,
            PortSettingsPatch {
                edge: Some(EdgeMode::Auto),
                bpdu_guard: Some(false),
                enabled: Some(true),
                ..p()
            }
        )
        .is_empty()
    );
    assert!(
        xg(
            1,
            PortSettingsPatch {
                edge: Some(EdgeMode::Disabled),
                native_vlan: Some(Some(4)),
                ..p()
            }
        )
        .is_empty()
    );

    // The opposite values must fail on the same captured state.
    let missing = pm(
        9,
        PortSettingsPatch {
            edge: Some(EdgeMode::Auto),
            bpdu_guard: Some(false),
            ..p()
        },
    );
    assert_eq!(missing.len(), 2, "{missing:?}");
    assert_eq!(
        pm(
            9,
            PortSettingsPatch {
                stp: Some(false),
                ..p()
            }
        )
        .len(),
        1
    );
    assert!(
        !xg(
            3,
            PortSettingsPatch {
                enabled: Some(false),
                ..p()
            }
        )
        .is_empty()
    );
    assert!(
        !xg(
            3,
            PortSettingsPatch {
                poe: Some(PoeMode::Off),
                ..p()
            }
        )
        .is_empty()
    );
    assert!(
        !xg(
            7,
            PortSettingsPatch {
                tagged_vlans: Some(vec![2, 3]),
                ..p()
            }
        )
        .is_empty()
    );
    assert!(
        !xg(
            3,
            PortSettingsPatch {
                native_vlan: Some(Some(4)),
                ..p()
            }
        )
        .is_empty()
    );

    let shut = XG10_RUNNING.replace(
        "interface te3\n switchport mode hybrid\n",
        "interface te3\n shutdown\n switchport mode hybrid\n",
    );
    assert!(
        effects_on_fixture(
            Model::Xg10,
            XG10_CFG,
            &shut,
            XG10_POE,
            &port_change(
                3,
                PortSettingsPatch {
                    enabled: Some(false),
                    ..p()
                }
            )
        )
        .is_empty()
    );
}

// The forms seen on the XG 10 by it_effect_forms, put into the te3 block of the
// fixture: each must pass, and the fixture block without it must fail.
#[test]
fn effects_strict_forms() {
    let te3_head = "interface te3\n switchport mode hybrid\n";
    let te3_running = |extra: &str| XG10_RUNNING.replace(te3_head, &format!("{te3_head}{extra}"));
    let xg = |running: &str, patch: PortSettingsPatch| {
        effects_on_fixture(
            Model::Xg10,
            XG10_CFG,
            running,
            XG10_POE,
            &port_change(3, patch),
        )
    };
    let stp_off = |edge, bpdu_guard| PortSettingsPatch {
        stp: Some(false),
        edge: Some(edge),
        bpdu_guard: Some(bpdu_guard),
        ..Default::default()
    };

    let both = te3_running(" no spanning-tree\n spanning-tree edge\n spanning-tree bpdu-guard\n");
    assert!(xg(&both, stp_off(EdgeMode::Enabled, true)).is_empty());
    let missing = xg(
        &te3_running(" no spanning-tree\n"),
        stp_off(EdgeMode::Enabled, true),
    );
    assert_eq!(
        missing,
        vec!["port 3: edge".to_string(), "port 3: bpdu_guard".to_string()]
    );
    assert!(
        xg(
            &te3_running(" no spanning-tree\n"),
            stp_off(EdgeMode::Auto, false)
        )
        .is_empty()
    );
    assert_eq!(
        xg(&both, stp_off(EdgeMode::Auto, false)),
        vec!["port 3: edge".to_string(), "port 3: bpdu_guard".to_string()]
    );
    // The form of `edge disabled` with STP off is not known; the read-back checks it.
    assert!(
        xg(
            &te3_running(" no spanning-tree\n"),
            stp_off(EdgeMode::Disabled, false)
        )
        .is_empty()
    );

    let no_native = XG10_RUNNING.replace(
        "interface te3\n switchport mode hybrid\n switchport hybrid pvid 3\n \
switchport hybrid allowed vlan add 3 untagged\n",
        "interface te3\n switchport mode hybrid\n switchport hybrid allowed vlan add 3 tagged\n",
    );
    assert_ne!(no_native, XG10_RUNNING);
    let native_none = || PortSettingsPatch {
        native_vlan: Some(None),
        tagged_vlans: Some(vec![3]),
        ..Default::default()
    };
    assert!(xg(&no_native, native_none()).is_empty());
    assert_eq!(
        xg(XG10_RUNNING, native_none()),
        vec![
            "port 3: native_vlan".to_string(),
            "port 3: tagged_vlans".to_string()
        ]
    );
    let native_1 = no_native.replace(
        "interface te3\n switchport mode hybrid\n switchport hybrid allowed vlan add 3 tagged\n \
switchport hybrid allowed vlan remove 1\n",
        "interface te3\n switchport mode hybrid\n switchport hybrid allowed vlan add 3 tagged\n",
    );
    assert_ne!(native_1, no_native);
    assert_eq!(
        xg(&native_1, native_none()),
        vec!["port 3: native_vlan".to_string()]
    );

    let empty_name = || PortSettingsPatch {
        name: Some(String::new()),
        ..Default::default()
    };
    let no_description = XG10_RUNNING.replace(
        "interface te3\n switchport mode hybrid\n switchport hybrid pvid 3\n \
switchport hybrid allowed vlan add 3 untagged\n switchport hybrid allowed vlan remove 1\n \
switchport forbidden vlan add 1-2,4-8\n authentication guest-vlan\n flowcontrol on\n \
description \"Port 3\"\n",
        "interface te3\n switchport mode hybrid\n switchport hybrid pvid 3\n \
switchport hybrid allowed vlan add 3 untagged\n switchport hybrid allowed vlan remove 1\n \
switchport forbidden vlan add 1-2,4-8\n authentication guest-vlan\n flowcontrol on\n",
    );
    assert_ne!(no_description, XG10_RUNNING);
    assert!(xg(&no_description, empty_name()).is_empty());
    assert_eq!(
        xg(XG10_RUNNING, empty_name()),
        vec!["port 3: name".to_string()]
    );
}

#[test]
fn effects_global_fixture_forms() {
    let check = |patch: GlobalSettingsPatch| {
        effects_on_fixture(
            Model::Xg10,
            XG10_CFG,
            XG10_RUNNING,
            XG10_POE,
            &Change::Global { patch },
        )
    };
    assert!(
        check(GlobalSettingsPatch {
            vlans: Some((1..=8).collect()),
            stp_version: Some("rstp".to_string()),
            stp_priority: Some(32768),
            bpdu_action: Some(BpduAction::Flooding),
        })
        .is_empty()
    );
    assert!(
        !check(GlobalSettingsPatch {
            vlans: Some((1..=7).collect()),
            ..Default::default()
        })
        .is_empty()
    );
    assert!(
        !check(GlobalSettingsPatch {
            bpdu_action: Some(BpduAction::Filtering),
            ..Default::default()
        })
        .is_empty()
    );
    assert_eq!(
        syscfg_get(&syscfg_parse(XG10_CFG), "switch.stp.priority"),
        Some("32768")
    );
    let cfg =
        syscfg_parse(&XG10_CFG.replace("switch.stp.priority=32768", "switch.stp.priority=4096"));
    let running = XG10_RUNNING.replace(
        "storm-control unit level\n",
        "storm-control unit level\nspanning-tree priority 4096\n",
    );
    let change = Change::Global {
        patch: GlobalSettingsPatch {
            stp_priority: Some(4096),
            ..Default::default()
        },
    };
    assert!(
        change_effects_check(
            Model::Xg10,
            &change,
            &cfg,
            &Overlay::default(),
            &running,
            None
        )
        .is_empty()
    );
    assert!(
        !change_effects_check(
            Model::Xg10,
            &change,
            &cfg,
            &Overlay::default(),
            XG10_RUNNING,
            None
        )
        .is_empty()
    );
}

const MGMT_CHECK: &str = "/etc/persistent/cfg/mgmt";

#[tokio::test(start_paused = true)]
async fn save_waits_for_mgmt() {
    let mut sw = FakeSwitch::xg10();
    let patch = PortSettingsPatch {
        name: Some("DUT 3".to_string()),
        ..Default::default()
    };
    sw.syscfg_after = syscfg_patched(XG10_CFG, Model::Xg10, 3, &patch);
    sw.running_after = XG10_RUNNING.replace("description \"Port 3\"", "description \"DUT 3\"");
    sw.mgmt_empty_reads = 2;
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    let begin = tokio::time::Instant::now();
    let result = change_apply(&fake, &ctx_xg10(), &store, &name_change(3, "DUT 3"))
        .await
        .unwrap();
    assert!(result.readback_ok);
    assert_eq!(begin.elapsed(), std::time::Duration::from_secs(10));
    let scripts = fake.scripts_get();
    let checks: Vec<usize> = scripts
        .iter()
        .enumerate()
        .filter(|(_, s)| s.contains(MGMT_CHECK))
        .map(|(i, _)| i)
        .collect();
    let save = scripts
        .iter()
        .position(|s| s.contains("save-config"))
        .unwrap();
    let put = scripts
        .iter()
        .position(|s| s.starts_with("#file_put"))
        .unwrap();
    assert_eq!(checks.len(), 3);
    assert!(checks.iter().all(|i| *i < put && put < save), "{scripts:?}");
}

#[tokio::test(start_paused = true)]
async fn save_refused_while_mgmt_empty() {
    let mut sw = FakeSwitch::xg10();
    sw.mgmt_empty_reads = usize::MAX;
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    let begin = tokio::time::Instant::now();
    let result = change_apply(&fake, &ctx_xg10(), &store, &name_change(3, "DUT 3")).await;
    match result {
        Err(ChangeError::SaveFailed(text)) => assert_eq!(
            text,
            "switch not ready to save: /etc/persistent/cfg/mgmt is empty"
        ),
        other => panic!("expected SaveFailed, got {other:?}"),
    }
    assert!(begin.elapsed() <= std::time::Duration::from_secs(90));
    assert!(
        fake.file_puts_get().is_empty(),
        "no write before the switch is ready"
    );
    assert!(!scripts_any(&fake, "apply-config"));
    assert!(!scripts_any(&fake, "save-config"));
    assert!(store.backup_list("xg10").is_empty());
}

#[tokio::test(start_paused = true)]
async fn restore_waits_for_mgmt() {
    let mut sw = FakeSwitch::xg10();
    sw.mgmt_empty_reads = usize::MAX;
    let fake = fake_switch_transport(sw);
    let (_dir, store) = store_temp();
    store
        .backup_save(
            "xg10",
            XG10_CFG,
            &overlay_render(&Overlay::default(), Model::Xg10),
        )
        .unwrap();
    let stamp = store.backup_list("xg10").remove(0);
    let result = backup_restore(&fake, &ctx_xg10(), &store, &stamp).await;
    assert!(
        matches!(result, Err(ChangeError::SaveFailed(_))),
        "{result:?}"
    );
    assert!(
        fake.file_puts_get().is_empty(),
        "no write before the switch is ready"
    );
    assert!(!scripts_any(&fake, "save-config"));
}
