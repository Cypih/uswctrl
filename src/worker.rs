// The device worker: one task per switch that owns its SSH access, polls it and runs requests one at a time.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, MissedTickBehavior};

use crate::backup::{BackupStore, backup_restore};
use crate::change::{
    Change, ChangeError, ChangeResult, OVERLAY_PATH, SwitchCtx, change_apply, change_plan,
    file_put_checked, file_read_optional, lock_check, lock_foreign_check, lock_read,
    output_describe, overlay_read, running_complete_check, running_config_read, script_run_checked,
    syscfg_read,
};
use crate::live::{Health, PortLive, probe_parse, probe_script_build};
use crate::locate::{LOCATE_OFF_SCRIPT, locate_ports_check, locate_script_build};
use crate::model::{Model, PoeAction, model_cli_port_name, model_port_count, poe_action_script};
use crate::overlay::{Overlay, overlay_parse};
use crate::runcfg::{runcfg_overlay_check, running_lines_clean};
use crate::settings::{
    GlobalSettings, PortSettings, SettingsError, global_settings_read, port_settings_read,
};
use crate::syscfg::{SysCfg, syscfg_parse};
use crate::transport::Transport;

pub const ACTION_PATH: &str = "/tmp/uswctl-action.scr";

const EVENTS_MAX: usize = 200;
const MEM_SAMPLE_PERIOD: Duration = Duration::from_secs(600);
// Seven days of one sample each ten minutes.
const MEM_SAMPLES_MAX: usize = 1008;
const REQUEST_QUEUE_LEN: usize = 32;
// A switch answers SSH 100 to 111 s after a reboot command (spec 5.4). The mark ends well
// after that, so that a reboot command that did nothing cannot hide a later unexpected
// reboot.
const REBOOT_MARK_HOLD: Duration = Duration::from_secs(600);
// `syswrapper.sh restart` is used only where the script has that case. The grep reads the
// same file that the command runs. The command runs detached and one second late, so that
// the SSH call returns its exit status before the reboot drops the session; the status
// therefore tells only that the switch accepted the command.
const REBOOT_SCRIPT: &str = "\
sw=$(command -v syswrapper.sh); \
if [ -n \"$sw\" ] && grep -q '^[[:space:]]*restart)' \"$sw\" 2>/dev/null; then \
cmd=\"$sw restart uswctl-request\"; else cmd=reboot; fi; \
(sleep 1; $cmd) </dev/null >/dev/null 2>&1 &";

enum ProbeError {
    Transport(anyhow::Error),
    Parse(anyhow::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    LinkDown,
    LinkUp,
    PoeOff,
    PoeOn,
    PoeCycle,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum EventKind {
    Reboot,
    CfgChangedExternally,
    OverlayRepaired,
    InformConnected(String),
    OfflineLogAdded(String),
    Unreachable,
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub kind: EventKind,
    pub at: SystemTime,
    /// True for a `Reboot` that followed a `reboot_run` request, so that the web page can
    /// tell it from an unexpected reboot. False for every other kind.
    pub requested: bool,
}

/// A running port locate: the blinking ports, in ascending order, and the time at which the
/// worker ends it. `end_failed` is true while the end failed and the worker tries it again
/// at each poll.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Locate {
    pub ports: Vec<u8>,
    pub until: SystemTime,
    pub end_failed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PortView {
    pub idx: u8,
    pub cli_name: String,
    pub protected: bool,
    pub live: PortLive,
    pub settings: PortSettings,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceState {
    pub firmware: String,
    pub firmware_tested: bool,
    pub uptime_s: f64,
    pub ports: Vec<PortView>,
    pub global: GlobalSettings,
    pub health: Health,
    pub mem_series: Vec<(SystemTime, u64)>,
    pub events: Vec<Event>,
    pub transient: BTreeMap<u8, Vec<Action>>,
    pub overlay_last_run: Option<SystemTime>,
    pub last_failure: Option<String>,
    pub locate: Option<Locate>,
}

type Reply<T> = oneshot::Sender<Result<T, ChangeError>>;

enum Request {
    Change {
        change: Change,
        dry_run: bool,
        reply: Reply<ChangeResult>,
    },
    Restore {
        stamp: String,
        reply: Reply<ChangeResult>,
    },
    Action {
        idx: u8,
        action: Action,
        reply: Reply<String>,
    },
    Reboot {
        reply: Reply<()>,
    },
    Locate {
        ports: Vec<u8>,
        duration: Duration,
        reply: Reply<(Locate, String)>,
    },
    LocateOff {
        reply: Reply<String>,
    },
}

/// The handle of one device worker. The worker stops when the last handle is dropped.
#[derive(Clone)]
pub struct DeviceHandle {
    sender: mpsc::Sender<Request>,
    state: Arc<Mutex<DeviceState>>,
}

fn stop_error_build() -> ChangeError {
    ChangeError::Unreachable("the device worker stopped".to_string())
}

impl DeviceHandle {
    // The state is read from a shared copy, not through the queue, so that a read does not
    // wait for a change that runs for many seconds.
    pub async fn state_get(&self) -> DeviceState {
        self.state.lock().expect("device state lock").clone()
    }

    async fn request_send<T>(
        &self,
        request_build: impl FnOnce(Reply<T>) -> Request,
    ) -> Result<T, ChangeError> {
        let (reply, answer) = oneshot::channel();
        self.sender
            .send(request_build(reply))
            .await
            .map_err(|_| stop_error_build())?;
        answer.await.map_err(|_| stop_error_build())?
    }

    pub async fn change_run(
        &self,
        change: Change,
        dry_run: bool,
    ) -> Result<ChangeResult, ChangeError> {
        self.request_send(|reply| Request::Change {
            change,
            dry_run,
            reply,
        })
        .await
    }

    pub async fn restore_run(&self, stamp: &str) -> Result<ChangeResult, ChangeError> {
        let stamp = stamp.to_string();
        self.request_send(|reply| Request::Restore { stamp, reply })
            .await
    }

    /// Runs a transient action and returns the text that it sent to the switch (see
    /// `action_script_build`), so that the caller can report it.
    pub async fn action_run(&self, idx: u8, action: Action) -> Result<String, ChangeError> {
        self.request_send(|reply| Request::Action { idx, action, reply })
            .await
    }

    pub async fn reboot_run(&self) -> Result<(), ChangeError> {
        self.request_send(|reply| Request::Reboot { reply }).await
    }

    /// Starts a locate of `ports` for `duration`, in place of a running locate, and returns
    /// the locate and the command that it sent to the switch.
    pub async fn locate_run(
        &self,
        ports: Vec<u8>,
        duration: Duration,
    ) -> Result<(Locate, String), ChangeError> {
        self.request_send(|reply| Request::Locate {
            ports,
            duration,
            reply,
        })
        .await
    }

    /// Ends the locate at once and returns the command that it sent to the switch.
    pub async fn locate_off_run(&self) -> Result<String, ChangeError> {
        self.request_send(|reply| Request::LocateOff { reply })
            .await
    }
}

/// The text that an action sends to the switch. For a link action it is the content of the
/// CLI file that the worker writes to `ACTION_PATH` and runs with `cli -e`; for a PoE action
/// it is the shell command. The caller checks `idx` against the model first.
pub fn action_script_build(model: Model, idx: u8, action: Action) -> String {
    let poe = |poe_action| poe_action_script(idx, poe_action);
    let link = |command: &str| {
        let name = model_cli_port_name(model, idx).unwrap_or_default();
        format!("interface {name}\n {command}\nexit\n")
    };
    match action {
        Action::LinkDown => link("shutdown"),
        Action::LinkUp => link("no shutdown"),
        Action::PoeOff => poe(PoeAction::Off),
        Action::PoeOn => poe(PoeAction::On),
        Action::PoeCycle => poe(PoeAction::Cycle),
    }
}

async fn script_apply(t: &dyn Transport, script: &str) -> Result<(), ChangeError> {
    let output = script_run_checked(t, script).await?;
    if output.status != 0 {
        return Err(ChangeError::ApplyFailed(output_describe(&output)));
    }
    Ok(())
}

// A tokio sleep, so that tests with a paused clock can reach the deadline.
async fn deadline_wait(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

fn action_cli_run_script_build() -> String {
    format!("cli -e {ACTION_PATH}; rc=$?; rm -f {ACTION_PATH}; exit $rc")
}

// A PoE cycle ends with the power on, as `swctrl poe restart` gives it back after about 6 s,
// so it adds nothing. Its effect on a port with PoE off is not known, so it also removes
// nothing.
fn transient_update(transient: &mut BTreeMap<u8, Vec<Action>>, idx: u8, action: Action) {
    let (add, remove) = match action {
        Action::LinkDown => (Some(Action::LinkDown), None),
        Action::LinkUp => (None, Some(Action::LinkDown)),
        Action::PoeOff => (Some(Action::PoeOff), None),
        Action::PoeOn => (None, Some(Action::PoeOff)),
        Action::PoeCycle => (None, None),
    };
    let list = transient.entry(idx).or_default();
    if let Some(added) = add
        && !list.contains(&added)
    {
        list.push(added);
    }
    if let Some(removed) = remove {
        list.retain(|a| *a != removed);
    }
    if list.is_empty() {
        transient.remove(&idx);
    }
}

// The status reads `Connected (<url>)` while `mcad` has a controller.
fn inform_url_get(status: &str) -> Option<String> {
    let rest = status.trim().strip_prefix("Connected")?.trim();
    let url = rest
        .strip_prefix('(')
        .and_then(|r| r.strip_suffix(')'))
        .unwrap_or(rest);
    Some(url.trim().to_string())
}

// An incomplete capture passes the absence checks of `runcfg_overlay_check`, so it must not
// decide whether the overlay is active.
async fn overlay_shown_check(
    t: &dyn Transport,
    model: Model,
    overlay: &Overlay,
) -> Result<bool, ChangeError> {
    let running = running_config_read(t).await?;
    if !running_complete_check(&running_lines_clean(&running), model, &[]) {
        return Err(ChangeError::ReadFailed(
            "the running config is incomplete".to_string(),
        ));
    }
    Ok(runcfg_overlay_check(&running, overlay, model))
}

struct Worker {
    t: Arc<dyn Transport>,
    ctx: SwitchCtx,
    protected: Vec<u8>,
    backup: BackupStore,
    shared: Arc<Mutex<DeviceState>>,
    state: DeviceState,
    cfg: SysCfg,
    overlay: Overlay,
    live: Vec<PortLive>,
    last_health: Option<Health>,
    settings_stale: bool,
    own_change: bool,
    unreachable: bool,
    reboot_mark: Option<Instant>,
    mem_next: Option<Instant>,
    locate_deadline: Option<Instant>,
}

impl Worker {
    fn state_publish(&self) {
        *self.shared.lock().expect("device state lock") = self.state.clone();
    }

    fn event_push(&mut self, kind: EventKind, requested: bool) {
        self.state.events.push(Event {
            kind,
            at: SystemTime::now(),
            requested,
        });
        if self.state.events.len() > EVENTS_MAX {
            let excess = self.state.events.len() - EVENTS_MAX;
            self.state.events.drain(..excess);
        }
    }

    fn failure_set(&mut self, text: String) {
        self.state.last_failure = Some(text);
    }

    fn ports_build(&mut self) {
        self.state.ports = self
            .live
            .iter()
            .map(|live| PortView {
                idx: live.idx,
                cli_name: model_cli_port_name(self.ctx.model, live.idx).unwrap_or_default(),
                protected: self.protected.contains(&live.idx),
                live: live.clone(),
                settings: port_settings_read(&self.cfg, &self.overlay, live.idx),
            })
            .collect();
    }

    async fn settings_refresh(&mut self) {
        let t = &*self.t;
        let read = async { Ok::<_, ChangeError>((syscfg_read(t).await?, overlay_read(t).await?)) };
        match read.await {
            Ok((cfg_text, overlay_text)) => {
                self.cfg = syscfg_parse(&cfg_text);
                self.overlay = overlay_parse(&overlay_text, self.ctx.model);
                self.state.global = global_settings_read(&self.cfg, &self.overlay);
                self.settings_stale = false;
                self.ports_build();
            }
            Err(e) => {
                self.settings_stale = true;
                self.failure_set(format!("settings read: {e}"));
            }
        }
    }

    async fn probe_read(&self) -> Result<(Health, Vec<PortLive>), ProbeError> {
        let output = self
            .t
            .script_run(&probe_script_build(self.ctx.lock_path.as_deref()))
            .await
            .map_err(ProbeError::Transport)?;
        probe_parse(&output.stdout).map_err(|e| {
            ProbeError::Parse(anyhow::anyhow!(
                "{e:#} (probe exit status {}: {})",
                output.status,
                output.stderr.trim()
            ))
        })
    }

    async fn probe_failure_note(&mut self, e: anyhow::Error) {
        self.failure_set(format!("probe: {e:#}"));
        if !self.unreachable {
            self.unreachable = true;
            self.event_push(EventKind::Unreachable, false);
        }
        self.t.master_reset().await;
    }

    fn mem_sample(&mut self, mem_avail_kb: u64) {
        let now = Instant::now();
        if self.mem_next.is_some_and(|next| now < next) {
            return;
        }
        self.mem_next = Some(now + MEM_SAMPLE_PERIOD);
        self.state
            .mem_series
            .push((SystemTime::now(), mem_avail_kb));
        if self.state.mem_series.len() > MEM_SAMPLES_MAX {
            let excess = self.state.mem_series.len() - MEM_SAMPLES_MAX;
            self.state.mem_series.drain(..excess);
        }
    }

    fn health_compare(&mut self, prev: Option<&Health>, health: &Health) -> bool {
        let prev_connected = prev.and_then(|p| inform_url_get(&p.inform_status));
        if let Some(url) = inform_url_get(&health.inform_status)
            && prev_connected.as_ref() != Some(&url)
        {
            self.event_push(EventKind::InformConnected(url), false);
        }
        let Some(prev) = prev else {
            return false;
        };

        let rebooted = health.uptime_s < prev.uptime_s;
        if rebooted {
            let requested = self
                .reboot_mark
                .take()
                .is_some_and(|until| Instant::now() <= until);
            self.state.transient.clear();
            self.locate_clear();
            self.event_push(EventKind::Reboot, requested);
        }
        if health.cfg_md5 != prev.cfg_md5 && !self.own_change {
            self.event_push(EventKind::CfgChangedExternally, false);
            self.settings_stale = true;
        }
        let known: BTreeSet<&String> = prev.offline_files.iter().collect();
        let added: Vec<String> = health
            .offline_files
            .iter()
            .filter(|f| !known.contains(f))
            .cloned()
            .collect();
        for file in added {
            self.event_push(EventKind::OfflineLogAdded(file), false);
        }
        rebooted
    }

    async fn poll_run(&mut self) {
        let (health, live) = match self.probe_read().await {
            Ok(read) => read,
            Err(ProbeError::Transport(e)) => {
                self.probe_failure_note(e).await;
                self.state_publish();
                return;
            }
            // The switch answered, so it is reachable: mca-dump gives no port table for a
            // while after a boot. The last good state stays until a probe parses.
            Err(ProbeError::Parse(e)) => {
                self.failure_set(format!("probe: {e:#}"));
                self.state_publish();
                return;
            }
        };
        self.unreachable = false;
        let prev = self.last_health.take();
        let rebooted = self.health_compare(prev.as_ref(), &health);
        self.own_change = false;
        self.mem_sample(health.mem_avail_kb);

        self.state.firmware = health.firmware.clone();
        self.state.firmware_tested = self.ctx.tested_firmware.contains(&health.firmware);
        self.state.uptime_s = health.uptime_s;
        self.state.health = health.clone();
        self.last_health = Some(health);
        self.live = live;
        if self.settings_stale {
            self.settings_refresh().await;
        } else {
            self.ports_build();
        }
        self.state_publish();

        if rebooted {
            self.t.master_reset().await;
            self.overlay_repair_run().await;
        }
        if self.state.locate.as_ref().is_some_and(|l| l.end_failed) {
            self.locate_end_run().await;
        }
    }

    // Restores the switch's own saved overlay, so it takes no lock and runs on an untested
    // firmware too (spec 11). It still waits while another agent holds the lock (A4): that
    // agent can be in the middle of a change by hand. The lock text of the last probe saves
    // one SSH call per check; without a parsed probe, for example early after a boot, the
    // lock file is read.
    async fn overlay_repair(&mut self) -> Result<(), ChangeError> {
        let t = &*self.t;
        let lock = match &self.last_health {
            Some(health) => health.lock.clone().unwrap_or_default(),
            None => lock_read(t, &self.ctx).await?,
        };
        if lock_foreign_check(&lock, &self.ctx) {
            return Ok(());
        }
        let model = self.ctx.model;
        let Some(text) = file_read_optional(t, OVERLAY_PATH).await? else {
            return Ok(());
        };
        let overlay = overlay_parse(&text, model);
        if overlay_shown_check(t, model, &overlay).await? {
            return Ok(());
        }
        let script = format!("cli -e {OVERLAY_PATH}");
        let output = script_run_checked(t, &script).await?;
        if output.status != 0 {
            return Err(ChangeError::ApplyFailed(format!(
                "{script}: {}",
                output_describe(&output)
            )));
        }
        self.state.overlay_last_run = Some(SystemTime::now());
        if !overlay_shown_check(t, model, &overlay).await? {
            return Err(ChangeError::NoEffect(vec![
                "overlay in the running config".to_string(),
            ]));
        }
        self.event_push(EventKind::OverlayRepaired, false);
        Ok(())
    }

    async fn overlay_repair_run(&mut self) {
        let result = self.overlay_repair().await;
        if let Err(e) = &result {
            self.failure_set(format!("overlay repair: {e}"));
        }
        self.transport_error_note(&result).await;
        self.state_publish();
    }

    // A call that did not reach the switch can have hung on a master connection to the
    // switch from before a reboot.
    async fn transport_error_note<T>(&self, result: &Result<T, ChangeError>) {
        if matches!(result, Err(ChangeError::Unreachable(_))) {
            self.t.master_reset().await;
        }
    }

    async fn change_run(
        &mut self,
        change: Change,
        dry_run: bool,
    ) -> Result<ChangeResult, ChangeError> {
        if dry_run {
            return change_plan(&*self.t, &self.ctx, &change).await;
        }
        let result = change_apply(&*self.t, &self.ctx, &self.backup, &change).await;
        self.own_write_note(&result);
        self.settings_refresh().await;
        result
    }

    async fn restore_run(&mut self, stamp: &str) -> Result<ChangeResult, ChangeError> {
        let result = backup_restore(&*self.t, &self.ctx, &self.backup, stamp).await;
        self.own_write_note(&result);
        self.settings_refresh().await;
        result
    }

    // A change that fails after its write still changes the checksum, so only the refusals
    // that come before any write leave the external-change watch armed.
    fn own_write_note(&mut self, result: &Result<ChangeResult, ChangeError>) {
        let refused_before_write = matches!(
            result,
            Err(ChangeError::Locked(_)
                | ChangeError::UntestedFirmware(_)
                | ChangeError::Settings(_))
        );
        if !refused_before_write {
            self.own_change = true;
        }
    }

    // Actions are allowed on an untested firmware (spec 11), so there is no firmware check.
    async fn action_run(&mut self, idx: u8, action: Action) -> Result<String, ChangeError> {
        let t = &*self.t;
        if idx == 0 || idx > model_port_count(self.ctx.model) {
            return Err(SettingsError::PortRange(idx).into());
        }
        lock_check(t, &self.ctx).await?;
        let script = action_script_build(self.ctx.model, idx, action);
        match action {
            Action::LinkDown | Action::LinkUp => {
                file_put_checked(t, ACTION_PATH, &script).await?;
                script_apply(t, &action_cli_run_script_build()).await?;
            }
            Action::PoeOff | Action::PoeOn | Action::PoeCycle => script_apply(t, &script).await?,
        }
        transient_update(&mut self.state.transient, idx, action);
        Ok(script)
    }

    async fn reboot_run(&mut self) -> Result<(), ChangeError> {
        let t = &*self.t;
        lock_check(t, &self.ctx).await?;
        script_apply(t, REBOOT_SCRIPT).await?;
        self.reboot_mark = Some(Instant::now() + REBOOT_MARK_HOLD);
        Ok(())
    }

    fn locate_clear(&mut self) {
        self.state.locate = None;
        self.locate_deadline = None;
    }

    // A locate changes LEDs only, so it is allowed on an untested firmware, like an action.
    // A set that ends unreachable can still have reached the switch, so the worker keeps the
    // locate and its deadline also then: the end at the deadline runs in every case.
    async fn locate_run(
        &mut self,
        ports: &[u8],
        duration: Duration,
    ) -> Result<(Locate, String), ChangeError> {
        let t = &*self.t;
        let ports = locate_ports_check(self.ctx.model, ports)?;
        lock_check(t, &self.ctx).await?;
        let script = locate_script_build(&ports);
        let result = match script_apply(t, &script).await {
            Err(e) if !matches!(e, ChangeError::Unreachable(_)) => return Err(e),
            result => result,
        };
        let locate = Locate {
            ports,
            until: SystemTime::now() + duration,
            end_failed: false,
        };
        self.state.locate = Some(locate.clone());
        self.locate_deadline = Some(Instant::now() + duration);
        result.map(|()| (locate, script))
    }

    // A stop and the end at the deadline only give the LEDs back, so they do not wait for a
    // foreign lock: a wait would leave the ports blinking for the time of the other agent's
    // work, with all other port LEDs dark.
    async fn locate_off_run(&mut self) -> Result<String, ChangeError> {
        script_apply(&*self.t, LOCATE_OFF_SCRIPT).await?;
        self.locate_clear();
        Ok(LOCATE_OFF_SCRIPT.to_string())
    }

    // A retry that fails again keeps the text of its first failure, so that it does not hide
    // the failure of a later request.
    async fn locate_end_run(&mut self) {
        self.locate_deadline = None;
        let result = script_apply(&*self.t, LOCATE_OFF_SCRIPT).await;
        match &result {
            Ok(()) => self.locate_clear(),
            Err(e) => {
                let text = format!("locate end: {e}");
                if let Some(locate) = self.state.locate.as_mut()
                    && !locate.end_failed
                {
                    locate.end_failed = true;
                    self.failure_set(text);
                }
            }
        }
        self.transport_error_note(&result).await;
        self.state_publish();
    }

    async fn result_note<T: Sync>(&mut self, result: &Result<T, ChangeError>) {
        if let Err(e) = result {
            self.failure_set(e.to_string());
        }
        self.transport_error_note(result).await;
    }

    async fn request_run(&mut self, request: Request) {
        match request {
            Request::Change {
                change,
                dry_run,
                reply,
            } => {
                let result = self.change_run(change, dry_run).await;
                self.result_note(&result).await;
                let _ = reply.send(result);
            }
            Request::Restore { stamp, reply } => {
                let result = self.restore_run(&stamp).await;
                self.result_note(&result).await;
                let _ = reply.send(result);
            }
            Request::Action { idx, action, reply } => {
                let result = self.action_run(idx, action).await;
                self.result_note(&result).await;
                let _ = reply.send(result);
            }
            Request::Reboot { reply } => {
                let result = self.reboot_run().await;
                self.result_note(&result).await;
                let _ = reply.send(result);
            }
            Request::Locate {
                ports,
                duration,
                reply,
            } => {
                let result = self.locate_run(&ports, duration).await;
                self.result_note(&result).await;
                let _ = reply.send(result);
            }
            Request::LocateOff { reply } => {
                let result = self.locate_off_run().await;
                self.result_note(&result).await;
                let _ = reply.send(result);
            }
        }
        self.state_publish();
    }

    // `biased` lets a due poll or overlay check run before the next request, so that a
    // stream of requests cannot stop the health watch.
    async fn loop_run(
        mut self,
        mut requests: mpsc::Receiver<Request>,
        poll: Duration,
        overlay_check: Duration,
    ) {
        self.settings_refresh().await;
        self.state_publish();
        let mut poll_tick = tokio::time::interval(poll);
        poll_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut overlay_tick = tokio::time::interval(overlay_check);
        overlay_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = poll_tick.tick() => self.poll_run().await,
                _ = overlay_tick.tick() => self.overlay_repair_run().await,
                _ = deadline_wait(self.locate_deadline) => self.locate_end_run().await,
                request = requests.recv() => match request {
                    Some(request) => self.request_run(request).await,
                    None => break,
                },
            }
        }
    }
}

/// Starts the worker of one switch on the current tokio runtime. A zero `poll` or
/// `overlay_check` is an error, because `tokio::time::interval` panics on it inside the task.
pub fn worker_spawn(
    t: Arc<dyn Transport>,
    ctx: SwitchCtx,
    protected: Vec<u8>,
    backup: BackupStore,
    poll: Duration,
    overlay_check: Duration,
) -> anyhow::Result<DeviceHandle> {
    if poll.is_zero() || overlay_check.is_zero() {
        anyhow::bail!(
            "worker of {}: poll ({poll:?}) and overlay check ({overlay_check:?}) must not be zero",
            ctx.name
        );
    }
    let empty_cfg = syscfg_parse("");
    let empty_overlay = Overlay::default();
    let state = DeviceState {
        firmware: String::new(),
        firmware_tested: false,
        uptime_s: 0.0,
        ports: Vec::new(),
        global: global_settings_read(&empty_cfg, &empty_overlay),
        health: Health::default(),
        mem_series: Vec::new(),
        events: Vec::new(),
        transient: BTreeMap::new(),
        overlay_last_run: None,
        last_failure: None,
        locate: None,
    };
    let shared = Arc::new(Mutex::new(state.clone()));
    let (sender, requests) = mpsc::channel(REQUEST_QUEUE_LEN);
    let worker = Worker {
        t,
        ctx,
        protected,
        backup,
        shared: shared.clone(),
        state,
        cfg: empty_cfg,
        overlay: empty_overlay,
        live: Vec::new(),
        last_health: None,
        settings_stale: true,
        own_change: false,
        unreachable: false,
        reboot_mark: None,
        mem_next: None,
        locate_deadline: None,
    };
    tokio::spawn(worker.loop_run(requests, poll, overlay_check));
    Ok(DeviceHandle {
        sender,
        state: shared,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inform_url_get_forms() {
        assert_eq!(
            inform_url_get("Connected (http://192.0.2.114:8080/inform)").as_deref(),
            Some("http://192.0.2.114:8080/inform")
        );
        assert_eq!(inform_url_get("Connected").as_deref(), Some(""));
        assert_eq!(inform_url_get("Timeout (http://unifi:8080/inform)"), None);
        assert_eq!(inform_url_get(""), None);
    }

    #[test]
    fn transient_update_add_and_remove() {
        let mut transient = BTreeMap::new();
        transient_update(&mut transient, 1, Action::LinkDown);
        transient_update(&mut transient, 1, Action::PoeOff);
        transient_update(&mut transient, 1, Action::LinkDown);
        assert_eq!(
            transient.get(&1),
            Some(&vec![Action::LinkDown, Action::PoeOff])
        );
        transient_update(&mut transient, 1, Action::PoeCycle);
        transient_update(&mut transient, 1, Action::PoeOn);
        assert_eq!(transient.get(&1), Some(&vec![Action::LinkDown]));
        transient_update(&mut transient, 1, Action::LinkUp);
        assert!(transient.is_empty());
        transient_update(&mut transient, 2, Action::PoeOn);
        assert!(transient.is_empty());
    }
}
