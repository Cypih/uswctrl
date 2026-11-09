// Backups of system.cfg and the overlay on this host, one directory per change, and restore.

use std::fs::{DirBuilder, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;

use crate::change::{
    ChangeError, ChangeResult, ConfigTexts, SwitchCtx, config_write, firmware_check, flash_read,
    lines_diff, lock_check, overlay_read, running_complete_check, running_config_read, syscfg_read,
};
use crate::overlay::{overlay_parse, overlay_render};
use crate::runcfg::{runcfg_overlay_check, running_lines_clean};
use crate::syscfg::{syscfg_diff, syscfg_parse};
use crate::transport::Transport;

const SYSCFG_FILE: &str = "system.cfg";
const OVERLAY_FILE: &str = "uswctl-overlay.scr";
const SECS_PER_DAY: u64 = 86_400;
// A stamp has a resolution of one second, so a retry one second later gets a new stamp.
const SAVE_ATTEMPTS: usize = 4;
const SAVE_RETRY_WAIT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct BackupStore {
    pub dir: PathBuf,
}

// Converts days since 1970-01-01 to a proleptic Gregorian date (algorithm "civil_from_days"
// by Howard Hinnant), so that the crate needs no date library for one format.
fn days_civil_convert(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

/// The UTC time `secs` after the Unix epoch as `YYYYMMDDTHHMMSSZ`.
pub fn stamp_format(secs: u64) -> String {
    let (year, month, day) = days_civil_convert(secs / SECS_PER_DAY);
    let rest = secs % SECS_PER_DAY;
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

fn stamp_check(stamp: &str) -> bool {
    let bytes = stamp.as_bytes();
    bytes.len() == 16
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 => *b == b'T',
            15 => *b == b'Z',
            _ => b.is_ascii_digit(),
        })
}

// The switch name becomes a directory name, so a name with a path separator or a dot
// name could write outside the backup directory.
fn switch_name_check(switch: &str) -> anyhow::Result<()> {
    if switch.is_empty() || switch == "." || switch == ".." || switch.contains('/') {
        anyhow::bail!("switch name {switch:?} is not a valid directory name");
    }
    Ok(())
}

fn file_write_private(path: &Path, text: &str) -> anyhow::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("write {}", path.display()))
}

fn secs_now_get() -> anyhow::Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before 1970")?
        .as_secs())
}

// The stamp directory is created without `recursive`, so that it fails with
// `AlreadyExists` instead of reusing the directory of an earlier backup.
fn stamp_dir_create(switch_dir: &Path) -> anyhow::Result<PathBuf> {
    for attempt in 0..SAVE_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(SAVE_RETRY_WAIT);
        }
        let path = switch_dir.join(stamp_format(secs_now_get()?));
        match DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("create {}", path.display())),
        }
    }
    anyhow::bail!(
        "no free backup name in {} after {SAVE_ATTEMPTS} attempts",
        switch_dir.display()
    )
}

impl BackupStore {
    pub fn backup_save(
        &self,
        switch: &str,
        syscfg: &str,
        overlay: &str,
    ) -> anyhow::Result<PathBuf> {
        switch_name_check(switch)?;
        let switch_dir = self.dir.join(switch);
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&switch_dir)
            .with_context(|| format!("create {}", switch_dir.display()))?;
        let path = stamp_dir_create(&switch_dir)?;
        file_write_private(&path.join(SYSCFG_FILE), syscfg)?;
        file_write_private(&path.join(OVERLAY_FILE), overlay)?;
        Ok(path)
    }

    pub fn backup_list(&self, switch: &str) -> Vec<String> {
        if switch_name_check(switch).is_err() {
            return Vec::new();
        }
        let Ok(entries) = std::fs::read_dir(self.dir.join(switch)) else {
            return Vec::new();
        };
        let mut stamps: Vec<String> = entries
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| stamp_check(name))
            .collect();
        // The stamp format sorts by time as plain text.
        stamps.sort_unstable_by(|a, b| b.cmp(a));
        stamps
    }

    /// The `system.cfg` and overlay texts of one backup.
    pub fn backup_load(&self, switch: &str, stamp: &str) -> anyhow::Result<(String, String)> {
        switch_name_check(switch)?;
        if !stamp_check(stamp) {
            anyhow::bail!("backup name {stamp:?} is not a YYYYMMDDTHHMMSSZ time stamp");
        }
        let path = self.dir.join(switch).join(stamp);
        let read = |name: &str| {
            let file = path.join(name);
            std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))
        };
        Ok((read(SYSCFG_FILE)?, read(OVERLAY_FILE)?))
    }
}

/// Writes a backup back to the switch with the steps of `change_apply`. The overlay is
/// always written and run, because a switch after a factory reset has none.
pub async fn backup_restore(
    t: &dyn Transport,
    ctx: &SwitchCtx,
    backup: &BackupStore,
    stamp: &str,
) -> Result<ChangeResult, ChangeError> {
    let (syscfg, overlay) = backup
        .backup_load(&ctx.name, stamp)
        .map_err(|e| ChangeError::BackupFailed(format!("{e:#}")))?;
    lock_check(t, ctx).await?;
    firmware_check(t, ctx).await?;
    let current = syscfg_read(t).await?;
    let current_overlay = overlay_read(t).await?;
    let keys = syscfg_diff(&syscfg_parse(&current), &syscfg_parse(&syscfg));
    let cli_lines = lines_diff(&current_overlay, &overlay);

    // The rendered overlay states every port, so that a run of it also takes back a filter
    // that the backup set; a missing file renders as the default overlay.
    let old_overlay = overlay_render(&overlay_parse(&current_overlay, ctx.model), ctx.model);
    config_write(
        t,
        &ConfigTexts {
            syscfg: Some(&current),
            overlay: Some(&old_overlay),
        },
        &ConfigTexts {
            syscfg: Some(&syscfg),
            overlay: Some(&overlay),
        },
    )
    .await?;

    let mut missing = Vec::new();
    let readback = syscfg_read(t).await?;
    if readback != syscfg {
        missing.push(format!("/tmp/system.cfg differs from backup {stamp}"));
    }
    let flash = flash_read(t).await?;
    if flash.trim_end_matches('\n') != syscfg.trim_end_matches('\n') {
        missing.push(format!("flash differs from backup {stamp}"));
    }
    let running = running_config_read(t).await?;
    let lines = running_lines_clean(&running);
    if !running_complete_check(&lines, ctx.model, &[]) {
        missing.push("running config incomplete".to_string());
    } else if !runcfg_overlay_check(&running, &overlay_parse(&overlay, ctx.model), ctx.model) {
        missing.push(format!("overlay of backup {stamp}"));
    }
    if !missing.is_empty() {
        return Err(ChangeError::NoEffect(missing));
    }
    backup
        .backup_save(&ctx.name, &readback, &overlay)
        .map_err(|e| ChangeError::BackupFailed(format!("{e:#}")))?;
    Ok(ChangeResult {
        keys,
        overlay_changed: current_overlay != overlay,
        cli_lines,
        readback_ok: true,
    })
}
