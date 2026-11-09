// Apply a settings change on a switch, check its effect and keep a backup (spec section 6.3).

use std::collections::BTreeSet;
use std::time::Duration;

use serde::Serialize;

use crate::backup::BackupStore;
use crate::model::{CLI_PAGER_OFF, Model, model_cli_port_name, model_port_count};
use crate::overlay::{Overlay, overlay_parse, overlay_render};
use crate::runcfg::{runcfg_overlay_check, runcfg_port_block_get, running_lines_clean};
use crate::settings::{
    EdgeMode, GlobalSettingsPatch, PoeMode, PortSettingsPatch, SettingsError, global_patch_apply,
    global_settings_read, port_patch_apply, port_settings_read,
};
use crate::syscfg::{KeyChange, SysCfg, syscfg_diff, syscfg_get, syscfg_parse, syscfg_render};
use crate::transport::{Output, Transport};

pub const SYSCFG_PATH: &str = "/tmp/system.cfg";
pub const OVERLAY_PATH: &str = "/etc/persistent/uswctl-overlay.scr";

const FIRMWARE_SCRIPT: &str = "mca-dump | jq -r .version";
const APPLY_SCRIPT: &str = "syswrapper.sh apply-config";
const SAVE_SCRIPT: &str = "syswrapper.sh save-config && syswrapper.sh cfg_save_check";
// `cfg_save_check` writes the flash only when this file is not empty, and it returns 0
// without a write when it is empty.
const SAVE_READY_PATH: &str = "/etc/persistent/cfg/mgmt";
const SAVE_READY_MARK: &str = "ready";
const SAVE_READY_POLL: Duration = Duration::from_secs(5);
const SAVE_READY_LIMIT: Duration = Duration::from_secs(90);
// `cfgmtd -r` reads the active flash slot. Its messages go to stderr, so that stdout holds
// the file only. The directory goes away on every path, and the exit status is the status
// of the read.
const FLASH_READ_SCRIPT: &str = "rm -rf /tmp/uswctl-read && mkdir -p /tmp/uswctl-read && \
cfgmtd -r -p /tmp/uswctl-read -f /tmp/uswctl-read/read.cfg >&2 && \
cat /tmp/uswctl-read/read.cfg; rc=$?; rm -rf /tmp/uswctl-read; exit $rc";
// Printed in place of the content of a missing file, so that a missing file and a failed
// read differ.
const NO_FILE_MARK: &str = "__USWCTL_NOFILE__";

// The CLI hides a value that equals its default. These are the defaults seen in
// `system.cfg` against `show running-config` on the XG 10 (firmware 7.5.15.17146).
const STP_VERSION_DEFAULT: &str = "rstp";
const STP_PRIORITY_DEFAULT: u32 = 32768;
const VLAN_DEFAULT: u16 = 1;

#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Port { idx: u8, patch: PortSettingsPatch },
    Global { patch: GlobalSettingsPatch },
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangeResult {
    pub keys: Vec<KeyChange>,
    pub overlay_changed: bool,
    pub cli_lines: Vec<String>,
    pub readback_ok: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ChangeError {
    #[error("an agent holds the lock on the switch: {0}")]
    Locked(String),
    #[error("firmware {0:?} is not in the list of tested firmware")]
    UntestedFirmware(String),
    #[error("the request is not valid: {0}")]
    Settings(#[from] SettingsError),
    #[error("the switch did not apply the change: {0}")]
    ApplyFailed(String),
    #[error("the switch did not save the configuration to flash: {0}")]
    SaveFailed(String),
    #[error("the switch does not show the effect of: {}", .0.join(", "))]
    NoEffect(Vec<String>),
    #[error("the switch is not reachable: {0}")]
    Unreachable(String),
    #[error("a read on the switch failed: {0}")]
    ReadFailed(String),
    #[error("the backup on this host failed: {0}")]
    BackupFailed(String),
}

#[derive(Debug, Clone)]
pub struct SwitchCtx {
    pub name: String,
    pub model: Model,
    pub lock_path: Option<String>,
    pub lock_token: String,
    pub tested_firmware: Vec<String>,
}

struct Planned {
    old_syscfg_text: String,
    old_overlay_text: String,
    cfg: SysCfg,
    overlay: Overlay,
    keys: Vec<KeyChange>,
    overlay_changed: bool,
    cli_lines: Vec<String>,
}

pub(crate) fn output_describe(output: &Output) -> String {
    let text = if output.stderr.trim().is_empty() {
        output.stdout.trim()
    } else {
        output.stderr.trim()
    };
    format!("exit status {}: {text}", output.status)
}

pub(crate) async fn script_run_checked(
    t: &dyn Transport,
    script: &str,
) -> Result<Output, ChangeError> {
    t.script_run(script)
        .await
        .map_err(|e| ChangeError::Unreachable(format!("{e:#}")))
}

pub(crate) async fn file_put_checked(
    t: &dyn Transport,
    path: &str,
    data: &str,
) -> Result<(), ChangeError> {
    t.file_put(path, data.as_bytes())
        .await
        .map_err(|e| ChangeError::Unreachable(format!("{e:#}")))
}

// Every text contains the empty string, so an empty token must never match a lock.
pub(crate) fn lock_foreign_check(lock: &str, ctx: &SwitchCtx) -> bool {
    let text = lock.trim();
    let own = !ctx.lock_token.is_empty() && text.contains(ctx.lock_token.as_str());
    !text.is_empty() && !own
}

// Without a lock path the switch has no lock, and an empty text never counts as foreign.
pub(crate) async fn lock_read(t: &dyn Transport, ctx: &SwitchCtx) -> Result<String, ChangeError> {
    let Some(path) = &ctx.lock_path else {
        return Ok(String::new());
    };
    Ok(file_read_optional(t, path).await?.unwrap_or_default())
}

pub(crate) async fn lock_check(t: &dyn Transport, ctx: &SwitchCtx) -> Result<(), ChangeError> {
    let lock = lock_read(t, ctx).await?;
    if lock_foreign_check(&lock, ctx) {
        return Err(ChangeError::Locked(lock.trim().to_string()));
    }
    Ok(())
}

pub(crate) async fn firmware_check(t: &dyn Transport, ctx: &SwitchCtx) -> Result<(), ChangeError> {
    let output = script_run_checked(t, FIRMWARE_SCRIPT).await?;
    let version = output.stdout.trim();
    if output.status == 0 && ctx.tested_firmware.iter().any(|v| v == version) {
        return Ok(());
    }
    Err(ChangeError::UntestedFirmware(version.to_string()))
}

async fn script_read(t: &dyn Transport, script: &str) -> Result<String, ChangeError> {
    let output = script_run_checked(t, script).await?;
    if output.status != 0 {
        return Err(ChangeError::ReadFailed(format!(
            "{script}: {}",
            output_describe(&output)
        )));
    }
    Ok(output.stdout)
}

pub(crate) async fn file_read_optional(
    t: &dyn Transport,
    path: &str,
) -> Result<Option<String>, ChangeError> {
    let script = format!("if [ -e {path} ]; then cat {path}; else echo {NO_FILE_MARK}; fi");
    let text = script_read(t, &script).await?;
    if text.trim_end() == NO_FILE_MARK {
        return Ok(None);
    }
    Ok(Some(text))
}

pub(crate) async fn syscfg_read(t: &dyn Transport) -> Result<String, ChangeError> {
    script_read(t, &format!("cat {SYSCFG_PATH}")).await
}

pub(crate) async fn flash_read(t: &dyn Transport) -> Result<String, ChangeError> {
    script_read(t, FLASH_READ_SCRIPT).await
}

// A missing overlay file is the normal state of a switch that never had a CLI-only
// setting, so it reads as an empty file.
pub(crate) async fn overlay_read(t: &dyn Transport) -> Result<String, ChangeError> {
    Ok(file_read_optional(t, OVERLAY_PATH)
        .await?
        .unwrap_or_default())
}

async fn syscfg_apply(t: &dyn Transport, text: &str) -> Result<(), ChangeError> {
    file_put_checked(t, SYSCFG_PATH, text).await?;
    let output = script_run_checked(t, APPLY_SCRIPT).await?;
    if output.status != 0 {
        return Err(ChangeError::ApplyFailed(format!(
            "{APPLY_SCRIPT}: {}",
            output_describe(&output)
        )));
    }
    Ok(())
}

async fn overlay_apply(t: &dyn Transport, text: &str) -> Result<(), ChangeError> {
    file_put_checked(t, OVERLAY_PATH, text).await?;
    let script = format!("cli -e {OVERLAY_PATH}");
    let output = script_run_checked(t, &script).await?;
    if output.status != 0 {
        return Err(ChangeError::ApplyFailed(format!(
            "{script}: {}",
            output_describe(&output)
        )));
    }
    Ok(())
}

// After a boot, the management file stays empty for a time after SSH answers. The wait
// runs before the first write, so that a switch that does not get ready keeps its old
// configuration instead of an applied change that no save can keep.
async fn save_ready_wait(t: &dyn Transport) -> Result<(), ChangeError> {
    let script =
        format!("if [ -s {SAVE_READY_PATH} ]; then echo {SAVE_READY_MARK}; else echo empty; fi");
    let begin = tokio::time::Instant::now();
    loop {
        if script_read(t, &script).await?.trim() == SAVE_READY_MARK {
            return Ok(());
        }
        if begin.elapsed() + SAVE_READY_POLL > SAVE_READY_LIMIT {
            return Err(ChangeError::SaveFailed(format!(
                "switch not ready to save: {SAVE_READY_PATH} is empty"
            )));
        }
        tokio::time::sleep(SAVE_READY_POLL).await;
    }
}

async fn config_save(t: &dyn Transport) -> Result<(), ChangeError> {
    let output = script_run_checked(t, SAVE_SCRIPT).await?;
    if output.status != 0 {
        return Err(ChangeError::SaveFailed(output_describe(&output)));
    }
    Ok(())
}

/// The texts of `system.cfg` and of the overlay file for one write. `None` leaves that
/// file as it is.
pub(crate) struct ConfigTexts<'a> {
    pub syscfg: Option<&'a str>,
    pub overlay: Option<&'a str>,
}

#[derive(Default)]
struct PutsDone {
    syscfg: bool,
    overlay: bool,
}

async fn config_steps_run(
    t: &dyn Transport,
    new: &ConfigTexts<'_>,
    puts: &mut PutsDone,
) -> Result<(), ChangeError> {
    if let Some(text) = new.syscfg {
        puts.syscfg = true;
        syscfg_apply(t, text).await?;
    }
    if let Some(text) = new.overlay {
        puts.overlay = true;
        overlay_apply(t, text).await?;
    }
    config_save(t).await
}

// Each step runs also when an earlier step failed, so that one failure does not leave the
// other file with the new text.
async fn config_rollback(t: &dyn Transport, old: &ConfigTexts<'_>, puts: &PutsDone) -> String {
    let mut failures = Vec::new();
    if puts.syscfg
        && let Some(text) = old.syscfg
        && let Err(e) = syscfg_apply(t, text).await
    {
        failures.push(format!("system.cfg: {e}"));
    }
    if puts.overlay
        && let Some(text) = old.overlay
        && let Err(e) = overlay_apply(t, text).await
    {
        failures.push(format!("overlay: {e}"));
    }
    if failures.is_empty() {
        "rollback: restored".to_string()
    } else {
        format!("rollback failed: {}", failures.join("; "))
    }
}

fn error_note_add(e: ChangeError, note: &str) -> ChangeError {
    match e {
        ChangeError::ApplyFailed(text) => ChangeError::ApplyFailed(format!("{text}; {note}")),
        ChangeError::SaveFailed(text) => ChangeError::SaveFailed(format!("{text}; {note}")),
        ChangeError::Unreachable(text) => ChangeError::Unreachable(format!("{text}; {note}")),
        other => other,
    }
}

/// Writes `new`, runs it and saves it to flash. A failed write leaves the new text in
/// `/tmp/system.cfg`, where the worker reads it as the setting and the next save of any
/// change writes it to flash (anti-goal A1). So on a failure after a put, the files that
/// got a put take the `old` text back and run again, best effort, and the error names the
/// result of that rollback.
pub(crate) async fn config_write(
    t: &dyn Transport,
    old: &ConfigTexts<'_>,
    new: &ConfigTexts<'_>,
) -> Result<(), ChangeError> {
    save_ready_wait(t).await?;
    let mut puts = PutsDone::default();
    let Err(e) = config_steps_run(t, new, &mut puts).await else {
        return Ok(());
    };
    if !puts.syscfg && !puts.overlay {
        return Err(e);
    }
    let note = config_rollback(t, old, &puts).await;
    Err(error_note_add(e, &note))
}

fn running_config_script_build() -> String {
    match CLI_PAGER_OFF {
        Some(pager_off) => format!("cli -c \"{pager_off}\" -c \"show running-config\""),
        None => "cli -c \"show running-config\"".to_string(),
    }
}

pub(crate) async fn running_config_read(t: &dyn Transport) -> Result<String, ChangeError> {
    script_read(t, &running_config_script_build()).await
}

async fn poe_show_read(t: &dyn Transport, idx: u8) -> Result<String, ChangeError> {
    script_read(t, &format!("swctrl poe show id {idx}")).await
}

// Both overlay renders have the same line layout for one model, so a line-by-line
// compare gives exactly the commands that change.
pub(crate) fn lines_diff(old: &str, new: &str) -> Vec<String> {
    let old_lines: Vec<&str> = old.lines().collect();
    new.lines()
        .enumerate()
        .filter(|(i, line)| old_lines.get(*i) != Some(line))
        .map(|(_, line)| line.to_string())
        .collect()
}

async fn plan_build(
    t: &dyn Transport,
    ctx: &SwitchCtx,
    change: &Change,
) -> Result<Planned, ChangeError> {
    let old_syscfg_text = syscfg_read(t).await?;
    let old_cfg = syscfg_parse(&old_syscfg_text);
    let old_overlay = overlay_parse(&overlay_read(t).await?, ctx.model);
    let mut cfg = old_cfg.clone();
    let mut overlay = old_overlay.clone();
    match change {
        Change::Port { idx, patch } => {
            port_patch_apply(&mut cfg, &mut overlay, ctx.model, *idx, patch)?
        }
        Change::Global { patch } => global_patch_apply(&mut cfg, &mut overlay, patch)?,
    }
    let overlay_changed = overlay != old_overlay;
    let cli_lines = if overlay_changed {
        lines_diff(
            &overlay_render(&old_overlay, ctx.model),
            &overlay_render(&overlay, ctx.model),
        )
    } else {
        Vec::new()
    };
    Ok(Planned {
        old_syscfg_text,
        old_overlay_text: overlay_render(&old_overlay, ctx.model),
        keys: syscfg_diff(&old_cfg, &cfg),
        cfg,
        overlay,
        overlay_changed,
        cli_lines,
    })
}

pub async fn change_plan(
    t: &dyn Transport,
    ctx: &SwitchCtx,
    change: &Change,
) -> Result<ChangeResult, ChangeError> {
    firmware_check(t, ctx).await?;
    let planned = plan_build(t, ctx, change).await?;
    Ok(ChangeResult {
        keys: planned.keys,
        overlay_changed: planned.overlay_changed,
        cli_lines: planned.cli_lines,
        readback_ok: false,
    })
}

pub async fn change_apply(
    t: &dyn Transport,
    ctx: &SwitchCtx,
    backup: &BackupStore,
    change: &Change,
) -> Result<ChangeResult, ChangeError> {
    lock_check(t, ctx).await?;
    firmware_check(t, ctx).await?;
    let planned = plan_build(t, ctx, change).await?;
    let result = ChangeResult {
        keys: planned.keys.clone(),
        overlay_changed: planned.overlay_changed,
        cli_lines: planned.cli_lines.clone(),
        readback_ok: true,
    };
    // A request for the values that the switch already has needs no flash write.
    if planned.keys.is_empty() && !planned.overlay_changed {
        return Ok(result);
    }

    let syscfg_text = syscfg_render(&planned.cfg);
    let overlay_text = overlay_render(&planned.overlay, ctx.model);
    let old = ConfigTexts {
        syscfg: Some(&planned.old_syscfg_text),
        overlay: Some(&planned.old_overlay_text),
    };
    let new = ConfigTexts {
        syscfg: (!planned.keys.is_empty()).then_some(syscfg_text.as_str()),
        overlay: planned.overlay_changed.then_some(overlay_text.as_str()),
    };
    config_write(t, &old, &new).await?;

    let readback = syscfg_read(t).await?;
    let mut missing = syscfg_readback_check(&syscfg_parse(&readback), &planned.keys, SYSCFG_PATH);
    let flash = flash_read(t).await?;
    missing.extend(syscfg_readback_check(
        &syscfg_parse(&flash),
        &planned.keys,
        "flash",
    ));
    let running = running_config_read(t).await?;
    let poe_show = match change {
        Change::Port {
            idx,
            patch: PortSettingsPatch { poe: Some(_), .. },
        } => Some(poe_show_read(t, *idx).await?),
        _ => None,
    };
    missing.extend(change_effects_check(
        ctx.model,
        change,
        &planned.cfg,
        &planned.overlay,
        &running,
        poe_show.as_deref(),
    ));
    if !missing.is_empty() {
        return Err(ChangeError::NoEffect(missing));
    }

    backup
        .backup_save(&ctx.name, &readback, &overlay_text)
        .map_err(|e| ChangeError::BackupFailed(format!("{e:#}")))?;
    Ok(result)
}

// `new` is `None` for a removed key, so the same compare also requires its absence.
fn syscfg_readback_check(readback: &SysCfg, keys: &[KeyChange], source: &str) -> Vec<String> {
    keys.iter()
        .filter(|k| syscfg_get(readback, &k.key) != k.new.as_deref())
        .map(|k| format!("{source} {}", k.key))
        .collect()
}

// Absence checks pass on a missing or cut output, so the output must show every port
// block of the model and, if the switch has more than VLAN 1, a `vlan` line.
pub(crate) fn running_complete_check(lines: &[String], model: Model, vlans: &[u16]) -> bool {
    let headers_ok = (1..=model_port_count(model)).all(|idx| {
        model_cli_port_name(model, idx)
            .is_none_or(|name| lines.iter().any(|l| *l == format!("interface {name}")))
    });
    let vlan_ok =
        vlans.iter().all(|id| *id == VLAN_DEFAULT) || lines.iter().any(|l| l.starts_with("vlan "));
    headers_ok && vlan_ok
}

// Accepts "4", "2-8" and "1-3,5-8".
fn vlan_list_parse(text: &str) -> BTreeSet<u16> {
    let mut ids = BTreeSet::new();
    for part in text.split(',') {
        let range = match part.split_once('-') {
            Some((low, high)) => low.trim().parse::<u16>().ok().zip(high.trim().parse().ok()),
            None => part.trim().parse::<u16>().ok().map(|id| (id, id)),
        };
        if let Some((low, high)) = range {
            ids.extend(low..=high);
        }
    }
    ids
}

fn port_vlans_shown_get(block: &[String], mode: &str) -> BTreeSet<u16> {
    let suffix = format!(" {mode}");
    block
        .iter()
        .filter_map(|line| {
            line.strip_prefix(" switchport hybrid allowed vlan add ")?
                .strip_suffix(suffix.as_str())
        })
        .flat_map(vlan_list_parse)
        .collect()
}

// The CLI hides `pvid 1`, so a port with native VLAN 1 and a port without a native VLAN
// both show no `pvid` line; only the second shows `remove 1` (XG 10).
fn port_native_check(block: &[String], native: Option<u16>) -> bool {
    let has = |line: &str| block.iter().any(|l| l == line);
    let pvid_shown = block
        .iter()
        .any(|l| l.starts_with(" switchport hybrid pvid "));
    let remove_1 = has(" switchport hybrid allowed vlan remove 1");
    match native {
        Some(VLAN_DEFAULT) => !pvid_shown && !remove_1,
        Some(id) => {
            has(&format!(" switchport hybrid pvid {id}"))
                && port_vlans_shown_get(block, "untagged").contains(&id)
        }
        None => !pvid_shown && remove_1 && port_vlans_shown_get(block, "untagged").is_empty(),
    }
}

// The CLI shows the edge lines with STP on and off. The form of `edge disabled` on a port
// with STP off was not seen on a device, so that one case passes here and only the
// system.cfg read-back checks it.
fn port_edge_check(block: &[String], stp: bool, edge: EdgeMode) -> bool {
    let has = |line: &str| block.iter().any(|l| l == line);
    match edge {
        EdgeMode::Enabled => has(" spanning-tree edge"),
        EdgeMode::Disabled => !stp || has(" no spanning-tree edge"),
        EdgeMode::Auto => !has(" spanning-tree edge") && !has(" no spanning-tree edge"),
    }
}

fn poe_opmode_get(poe_show: &str, idx: u8) -> Option<String> {
    let idx_text = idx.to_string();
    poe_show.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        if fields.next() != Some(idx_text.as_str()) {
            return None;
        }
        fields.next().map(str::to_string)
    })
}

fn port_cli_effects_check(
    model: Model,
    idx: u8,
    patch: &PortSettingsPatch,
    cfg: &SysCfg,
    overlay: &Overlay,
    running: &str,
) -> Vec<String> {
    let settings = port_settings_read(cfg, overlay, idx);
    let block = model_cli_port_name(model, idx)
        .map(|name| runcfg_port_block_get(running, &name))
        .unwrap_or_default();
    let has = |line: &str| block.iter().any(|l| l == line);
    let mut missing = Vec::new();
    let mut field_missing = |field: &str| missing.push(format!("port {idx}: {field}"));

    // ubntconf writes a '+' in a port name as '_', and the CLI shows no `description` line
    // for an empty name.
    let name_ok = if settings.name.is_empty() {
        !block.iter().any(|l| l.starts_with(" description "))
    } else {
        has(&format!(
            " description \"{}\"",
            settings.name.replace('+', "_")
        ))
    };
    if patch.name.is_some() && !name_ok {
        field_missing("name");
    }
    if patch.native_vlan.is_some() && !port_native_check(&block, settings.native_vlan) {
        field_missing("native_vlan");
    }
    if patch.tagged_vlans.is_some() {
        let expected: BTreeSet<u16> = settings.tagged_vlans.iter().copied().collect();
        if port_vlans_shown_get(&block, "tagged") != expected {
            field_missing("tagged_vlans");
        }
    }
    if patch.stp.is_some() && has(" no spanning-tree") == settings.stp {
        field_missing("stp");
    }
    if patch.edge.is_some() && !port_edge_check(&block, settings.stp, settings.edge) {
        field_missing("edge");
    }
    if patch.bpdu_guard.is_some() && has(" spanning-tree bpdu-guard") != settings.bpdu_guard {
        field_missing("bpdu_guard");
    }
    if patch.enabled.is_some() && has(" shutdown") == settings.enabled {
        field_missing("enabled");
    }
    if patch.bpdu_filter.is_some() && !runcfg_overlay_check(running, overlay, model) {
        field_missing("bpdu_filter");
    }
    missing
}

fn port_effects_check(
    model: Model,
    idx: u8,
    patch: &PortSettingsPatch,
    cfg: &SysCfg,
    overlay: &Overlay,
    running: &str,
    poe_show: Option<&str>,
) -> Vec<String> {
    let mut missing = Vec::new();
    let cli_checked = patch.name.is_some()
        || patch.native_vlan.is_some()
        || patch.tagged_vlans.is_some()
        || patch.stp.is_some()
        || patch.edge.is_some()
        || patch.bpdu_guard.is_some()
        || patch.enabled.is_some()
        || patch.bpdu_filter.is_some();
    if cli_checked {
        let lines = running_lines_clean(running);
        let header_shown = model_cli_port_name(model, idx)
            .is_some_and(|name| lines.contains(&format!("interface {name}")));
        // The BPDU filter check reads every port block of the overlay.
        let complete = patch.bpdu_filter.is_none() || running_complete_check(&lines, model, &[]);
        if !header_shown {
            missing.push(format!("port {idx}: block not shown"));
        } else if !complete {
            missing.push(format!("port {idx}: running config incomplete"));
        } else {
            missing.extend(port_cli_effects_check(
                model, idx, patch, cfg, overlay, running,
            ));
        }
    }
    if let Some(poe) = patch.poe {
        let expected = match poe {
            PoeMode::Auto => "Auto",
            PoeMode::Off => "Off",
        };
        let opmode = poe_show.and_then(|text| poe_opmode_get(text, idx));
        if opmode.as_deref() != Some(expected) {
            missing.push(format!("port {idx}: poe"));
        }
    }
    missing
}

// A global line that the CLI hides at its default value: no line passes only for the
// default, and a shown line must carry the expected value.
fn global_line_check(lines: &[String], prefix: &str, value: &str, default: &str) -> bool {
    let shown: Vec<&String> = lines.iter().filter(|l| l.starts_with(prefix)).collect();
    if shown.is_empty() {
        return value == default;
    }
    let expected = format!("{prefix}{value}");
    shown.iter().all(|l| **l == expected)
}

fn global_effects_check(
    model: Model,
    patch: &GlobalSettingsPatch,
    cfg: &SysCfg,
    overlay: &Overlay,
    running: &str,
) -> Vec<String> {
    let settings = global_settings_read(cfg, overlay);
    let lines = running_lines_clean(running);
    if !running_complete_check(&lines, model, &settings.vlans) {
        return vec!["global: running config incomplete".to_string()];
    }
    let mut missing = Vec::new();
    let mut field_missing = |field: &str| missing.push(format!("global: {field}"));

    if patch.stp_version.is_some()
        && !global_line_check(
            &lines,
            "spanning-tree mode ",
            &settings.stp_version,
            STP_VERSION_DEFAULT,
        )
    {
        field_missing("stp_version");
    }
    if patch.stp_priority.is_some()
        && !global_line_check(
            &lines,
            "spanning-tree priority ",
            &settings.stp_priority.to_string(),
            &STP_PRIORITY_DEFAULT.to_string(),
        )
    {
        field_missing("stp_priority");
    }
    if patch.vlans.is_some() {
        let mut shown: BTreeSet<u16> = lines
            .iter()
            .filter_map(|l| l.strip_prefix("vlan "))
            .flat_map(vlan_list_parse)
            .collect();
        shown.insert(VLAN_DEFAULT);
        let expected: BTreeSet<u16> = settings.vlans.iter().copied().collect();
        if shown != expected {
            field_missing("vlans");
        }
    }
    if patch.bpdu_action.is_some() && !runcfg_overlay_check(running, overlay, model) {
        field_missing("bpdu_action");
    }
    missing
}

/// The settings of `change` whose effect the running configuration (and, for PoE, the
/// output of `swctrl poe show`) does not show. `cfg` and `overlay` are the state after
/// the change. Only the settings that the change names are checked.
pub fn change_effects_check(
    model: Model,
    change: &Change,
    cfg: &SysCfg,
    overlay: &Overlay,
    running: &str,
    poe_show: Option<&str>,
) -> Vec<String> {
    match change {
        Change::Port { idx, patch } => {
            port_effects_check(model, *idx, patch, cfg, overlay, running, poe_show)
        }
        Change::Global { patch } => global_effects_check(model, patch, cfg, overlay, running),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vlan_list_parse_forms() {
        assert_eq!(vlan_list_parse("4"), BTreeSet::from([4]));
        assert_eq!(vlan_list_parse("2-4"), BTreeSet::from([2, 3, 4]));
        assert_eq!(
            vlan_list_parse("1-2,5,7-8"),
            BTreeSet::from([1, 2, 5, 7, 8])
        );
        assert!(vlan_list_parse("x").is_empty());
    }

    #[test]
    fn lines_diff_positional() {
        assert_eq!(lines_diff("a\nb\nc\n", "a\nB\nc\n"), vec!["B"]);
        assert_eq!(lines_diff("", "a\nb\n"), vec!["a", "b"]);
        assert!(lines_diff("a\n", "a\n").is_empty());
    }

    #[test]
    fn poe_opmode_get_reads_column() {
        let text = include_str!("../tests/fixtures/promax16/poe-show.txt");
        assert_eq!(poe_opmode_get(text, 1).as_deref(), Some("Auto"));
        assert_eq!(poe_opmode_get(text, 8).as_deref(), Some("Off"));
        assert_eq!(poe_opmode_get(text, 17), None);
    }

    #[test]
    fn global_line_check_default_hidden() {
        let none: Vec<String> = Vec::new();
        assert!(global_line_check(
            &none,
            "spanning-tree mode ",
            "rstp",
            "rstp"
        ));
        assert!(!global_line_check(
            &none,
            "spanning-tree mode ",
            "stp",
            "rstp"
        ));
        let shown = vec!["spanning-tree mode stp".to_string()];
        assert!(global_line_check(
            &shown,
            "spanning-tree mode ",
            "stp",
            "rstp"
        ));
        assert!(!global_line_check(
            &shown,
            "spanning-tree mode ",
            "rstp",
            "rstp"
        ));
    }
}
