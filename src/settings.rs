// Port and global switch settings, read from and written to system.cfg keys and the overlay.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer, Serialize};

use crate::model::{Model, model_port_count, port_disable_key};
use crate::overlay::Overlay;
use crate::syscfg::{
    SysCfg, syscfg_get, syscfg_keys_list, syscfg_remove, syscfg_rename, syscfg_set,
};

// IEEE 802.1D default bridge priority.
const STP_PRIORITY_DEFAULT: u32 = 32768;
const STP_PRIORITY_STEP: u32 = 4096;
const STP_PRIORITY_MAX: u32 = 61440;
const STP_VERSIONS: [&str; 2] = ["stp", "rstp"];
const VLAN_ID_MAX: u16 = 4094;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoeMode {
    Auto,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeMode {
    Auto,
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BpduAction {
    Flooding,
    Filtering,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortSettings {
    pub name: String,
    pub native_vlan: Option<u16>,
    pub tagged_vlans: Vec<u16>,
    pub poe: PoeMode,
    pub stp: bool,
    pub edge: EdgeMode,
    pub bpdu_guard: bool,
    pub bpdu_filter: bool,
    pub enabled: bool,
    pub vlan_all: bool,
}

// `native_vlan` needs three states: absent (no change), `null` (no native VLAN) and a
// VLAN id. Plain serde maps `null` and absent to the same `None`.
// An unknown field is refused: a typo would otherwise give an empty patch that succeeds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PortSettingsPatch {
    pub name: Option<String>,
    #[serde(
        deserialize_with = "option_double_deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub native_vlan: Option<Option<u16>>,
    pub tagged_vlans: Option<Vec<u16>>,
    pub poe: Option<PoeMode>,
    pub stp: Option<bool>,
    pub edge: Option<EdgeMode>,
    pub bpdu_guard: Option<bool>,
    pub bpdu_filter: Option<bool>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlobalSettings {
    pub stp_version: String,
    pub stp_priority: u32,
    pub bpdu_action: BpduAction,
    pub vlans: Vec<u16>,
}

// An unknown field is refused: a typo would otherwise give an empty patch that succeeds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GlobalSettingsPatch {
    pub stp_version: Option<String>,
    pub stp_priority: Option<u32>,
    pub bpdu_action: Option<BpduAction>,
    pub vlans: Option<Vec<u16>>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SettingsError {
    #[error("port {0} does not exist on this switch")]
    PortRange(u8),
    #[error("port {0} is in the port list more than once")]
    PortDuplicate(u8),
    #[error("the port list is empty")]
    PortsEmpty,
    #[error("VLAN {0} is not in the VLAN list of the switch")]
    VlanUnknown(u16),
    #[error("VLAN {0} cannot be the native VLAN and a tagged VLAN of the same port")]
    NativeAlsoTagged(u16),
    #[error("port name {0:?} contains a control character, a \" or a \\")]
    PortName(String),
    #[error("VLAN id {0} is outside 1 to 4094")]
    VlanRange(u16),
    #[error("VLAN {0} is the native VLAN of a port")]
    VlanInUse(u16),
    #[error("VLAN {0} is the management VLAN of the switch")]
    VlanManagement(u16),
    #[error("port {0} has no PoE")]
    PoeUnsupported(u8),
    #[error("STP version {0:?} is not stp or rstp")]
    StpVersion(String),
    #[error("STP priority {0} is not a multiple of 4096 from 0 to 61440")]
    StpPriority(u32),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct VlanTable {
    index_by_id: BTreeMap<u16, u32>,
}

impl VlanTable {
    pub fn index_get(&self, id: u16) -> Option<u32> {
        self.index_by_id.get(&id).copied()
    }

    pub fn ids_list(&self) -> Vec<u16> {
        self.index_by_id.keys().copied().collect()
    }

    fn entries_iter(&self) -> impl DoubleEndedIterator<Item = (u16, u32)> + '_ {
        self.index_by_id.iter().map(|(id, n)| (*id, *n))
    }
}

fn option_double_deserialize<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

fn vlan_key_split(key: &str) -> Option<(u32, &str)> {
    let (index, rest) = key.strip_prefix("switch.vlan.")?.split_once('.')?;
    Some((index.parse().ok()?, rest))
}

pub fn vlan_table_read(cfg: &SysCfg) -> VlanTable {
    let index_by_id = syscfg_keys_list(cfg)
        .into_iter()
        .filter_map(|key| {
            let (n, rest) = vlan_key_split(key)?;
            if rest != "id" {
                return None;
            }
            let id = syscfg_get(cfg, key)?.parse().ok()?;
            Some((id, n))
        })
        .collect();
    VlanTable { index_by_id }
}

fn vlan_index_next(cfg: &SysCfg) -> u32 {
    syscfg_keys_list(cfg)
        .into_iter()
        .filter_map(|key| vlan_key_split(key).map(|(n, _)| n))
        .max()
        .map_or(1, |n| n + 1)
}

fn port_key(idx: u8, field: &str) -> String {
    format!("switch.port.{idx}.{field}")
}

fn vlan_port_key(n: u32, idx: u8) -> String {
    format!("switch.vlan.{n}.port.{idx}.mode")
}

// Ports that the controller set to all VLANs have no membership lines. For such a port
// the VLAN's own `mode` key applies.
fn port_vlan_mode_get(cfg: &SysCfg, n: u32, idx: u8) -> Option<&str> {
    syscfg_get(cfg, &vlan_port_key(n, idx))
        .or_else(|| syscfg_get(cfg, &format!("switch.vlan.{n}.mode")))
}

struct PortVlans {
    native: Option<u16>,
    tagged: Vec<u16>,
}

fn port_vlans_read(cfg: &SysCfg, table: &VlanTable, idx: u8) -> PortVlans {
    let pvid = syscfg_get(cfg, &port_key(idx, "pvid")).and_then(|v| v.parse().ok());
    let native = pvid.or_else(|| {
        table
            .entries_iter()
            .find(|(_, n)| port_vlan_mode_get(cfg, *n, idx) == Some("untagged"))
            .map(|(id, _)| id)
    });
    let tagged = table
        .entries_iter()
        .filter(|(id, n)| Some(*id) != native && port_vlan_mode_get(cfg, *n, idx) == Some("tagged"))
        .map(|(id, _)| id)
        .collect();
    PortVlans { native, tagged }
}

fn port_vlan_all_get(cfg: &SysCfg, table: &VlanTable, idx: u8) -> bool {
    table
        .entries_iter()
        .all(|(_, n)| syscfg_get(cfg, &vlan_port_key(n, idx)).is_none())
}

fn ports_list(cfg: &SysCfg) -> BTreeSet<u8> {
    syscfg_keys_list(cfg)
        .into_iter()
        .filter_map(|key| {
            key.strip_prefix("switch.port.")?
                .split_once('.')?
                .0
                .parse()
                .ok()
        })
        .collect()
}

pub fn port_settings_read(cfg: &SysCfg, overlay: &Overlay, idx: u8) -> PortSettings {
    let table = vlan_table_read(cfg);
    let PortVlans {
        native: native_vlan,
        tagged: tagged_vlans,
    } = port_vlans_read(cfg, &table, idx);
    let value_get = |field: &str| syscfg_get(cfg, &port_key(idx, field));
    let poe = match value_get("poe") {
        Some("shutdown") => PoeMode::Off,
        _ => PoeMode::Auto,
    };
    let edge = match value_get("stp.edge") {
        Some("enabled") => EdgeMode::Enabled,
        Some("disabled") => EdgeMode::Disabled,
        _ => EdgeMode::Auto,
    };
    let (disable_key, disable_value) = port_disable_key(idx);
    PortSettings {
        name: value_get("name").unwrap_or_default().to_string(),
        native_vlan,
        tagged_vlans,
        poe,
        stp: value_get("stp.port_mode") == Some("enabled"),
        edge,
        bpdu_guard: value_get("stp.bpdu_guard") == Some("enabled"),
        bpdu_filter: overlay.port_bpdu_filter.contains(&idx),
        enabled: syscfg_get(cfg, &disable_key) != Some(disable_value.as_str()),
        vlan_all: port_vlan_all_get(cfg, &table, idx),
    }
}

// Returns the new native and tagged VLANs, or `None` if the patch leaves the membership
// as it is.
fn port_membership_plan(
    cfg: &SysCfg,
    table: &VlanTable,
    idx: u8,
    patch: &PortSettingsPatch,
) -> Result<Option<PortVlans>, SettingsError> {
    if patch.native_vlan.is_none() && patch.tagged_vlans.is_none() {
        return Ok(None);
    }
    let patch_ids = patch
        .native_vlan
        .flatten()
        .into_iter()
        .chain(patch.tagged_vlans.iter().flatten().copied());
    for id in patch_ids {
        if table.index_get(id).is_none() {
            return Err(SettingsError::VlanUnknown(id));
        }
    }

    let now = port_vlans_read(cfg, table, idx);
    let native = patch.native_vlan.unwrap_or(now.native);
    let tagged: Vec<u16> = match &patch.tagged_vlans {
        Some(ids) => ids
            .iter()
            .copied()
            .collect::<BTreeSet<u16>>()
            .into_iter()
            .collect(),
        None => now.tagged,
    };
    if let Some(id) = native.filter(|id| tagged.contains(id)) {
        return Err(SettingsError::NativeAlsoTagged(id));
    }
    Ok(Some(PortVlans { native, tagged }))
}

fn port_membership_write(cfg: &mut SysCfg, table: &VlanTable, idx: u8, vlans: &PortVlans) {
    let native = vlans.native;
    let tagged = &vlans.tagged;
    let pvid_key = port_key(idx, "pvid");
    match native {
        Some(id) => syscfg_set(cfg, &pvid_key, &id.to_string()),
        None => syscfg_remove(cfg, &pvid_key),
    }
    for (id, n) in table.entries_iter() {
        let mode = if native == Some(id) {
            "untagged"
        } else if tagged.contains(&id) {
            "tagged"
        } else {
            "exclude"
        };
        syscfg_set(cfg, &vlan_port_key(n, idx), mode);
    }
}

fn enabled_str(on: bool) -> &'static str {
    if on { "enabled" } else { "disabled" }
}

// The name goes into system.cfg as one line and into the CLI as a quoted `description`,
// so a control character, a quote or a backslash could end the value early.
fn port_name_check(name: &str) -> bool {
    !name
        .chars()
        .any(|c| c.is_control() || c == '"' || c == '\\')
}

pub fn port_patch_apply(
    cfg: &mut SysCfg,
    overlay: &mut Overlay,
    model: Model,
    idx: u8,
    patch: &PortSettingsPatch,
) -> Result<(), SettingsError> {
    if idx == 0 || idx > model_port_count(model) {
        return Err(SettingsError::PortRange(idx));
    }
    let table = vlan_table_read(cfg);
    let membership = port_membership_plan(cfg, &table, idx, patch)?;
    if let Some(name) = patch.name.as_ref().filter(|name| !port_name_check(name)) {
        return Err(SettingsError::PortName(name.clone()));
    }
    if patch.poe.is_some() && syscfg_get(cfg, &port_key(idx, "poe")).is_none() {
        return Err(SettingsError::PoeUnsupported(idx));
    }

    if let Some(name) = &patch.name {
        syscfg_set(cfg, &port_key(idx, "name"), name);
    }
    if let Some(vlans) = membership {
        port_membership_write(cfg, &table, idx, &vlans);
    }
    if let Some(poe) = patch.poe {
        let value = match poe {
            PoeMode::Auto => "auto",
            PoeMode::Off => "shutdown",
        };
        syscfg_set(cfg, &port_key(idx, "poe"), value);
    }
    if let Some(stp) = patch.stp {
        syscfg_set(cfg, &port_key(idx, "stp.port_mode"), enabled_str(stp));
    }
    if let Some(edge) = patch.edge {
        let value = match edge {
            EdgeMode::Auto => "auto",
            EdgeMode::Enabled => "enabled",
            EdgeMode::Disabled => "disabled",
        };
        syscfg_set(cfg, &port_key(idx, "stp.edge"), value);
    }
    if let Some(guard) = patch.bpdu_guard {
        syscfg_set(cfg, &port_key(idx, "stp.bpdu_guard"), enabled_str(guard));
    }
    // The controller writes no status key for an enabled port, so removal restores the
    // state that the controller itself would write.
    if let Some(enabled) = patch.enabled {
        let (key, value) = port_disable_key(idx);
        if enabled {
            syscfg_remove(cfg, &key);
        } else {
            syscfg_set(cfg, &key, &value);
        }
    }
    match patch.bpdu_filter {
        Some(true) => {
            overlay.port_bpdu_filter.insert(idx);
        }
        Some(false) => {
            overlay.port_bpdu_filter.remove(&idx);
        }
        None => {}
    }
    Ok(())
}

pub fn global_settings_read(cfg: &SysCfg, overlay: &Overlay) -> GlobalSettings {
    GlobalSettings {
        stp_version: syscfg_get(cfg, "switch.stp.version")
            .unwrap_or_default()
            .to_string(),
        stp_priority: syscfg_get(cfg, "switch.stp.priority")
            .and_then(|v| v.parse().ok())
            .unwrap_or(STP_PRIORITY_DEFAULT),
        bpdu_action: overlay.bpdu_action,
        vlans: vlan_table_read(cfg).ids_list(),
    }
}

fn vlans_removed_check(
    cfg: &SysCfg,
    table: &VlanTable,
    removed: &BTreeSet<u16>,
) -> Result<(), SettingsError> {
    let management = syscfg_get(cfg, "switch.managementvlan").and_then(|v| v.parse::<u16>().ok());
    if let Some(id) = management.filter(|id| removed.contains(id)) {
        return Err(SettingsError::VlanManagement(id));
    }
    let natives: BTreeSet<u16> = ports_list(cfg)
        .into_iter()
        .filter_map(|idx| port_vlans_read(cfg, table, idx).native)
        .collect();
    match removed.iter().find(|id| natives.contains(id)) {
        Some(id) => Err(SettingsError::VlanInUse(*id)),
        None => Ok(()),
    }
}

// The higher entries move down one index, so that the indices stay contiguous as the
// controller writes them.
fn vlan_remove(cfg: &mut SysCfg, n: u32) {
    let mut moves: Vec<(u32, String)> = Vec::new();
    let mut doomed: Vec<String> = Vec::new();
    for key in syscfg_keys_list(cfg) {
        match vlan_key_split(key) {
            Some((index, _)) if index == n => doomed.push(key.to_string()),
            Some((index, rest)) if index > n => moves.push((index, rest.to_string())),
            _ => {}
        }
    }
    for key in doomed {
        syscfg_remove(cfg, &key);
    }
    moves.sort_by_key(|(index, _)| *index);
    for (index, rest) in moves {
        syscfg_rename(
            cfg,
            &format!("switch.vlan.{index}.{rest}"),
            &format!("switch.vlan.{}.{rest}", index - 1),
        );
    }
}

// A new VLAN copies the VLAN-wide keys (IGMP and similar) of the highest tagged VLAN, so
// that it behaves like the VLANs that the controller created.
// Only ports with explicit membership lines get `exclude`; a port without lines keeps the
// all-VLANs profile of the controller.
fn vlan_add(cfg: &mut SysCfg, id: u16, ports: &BTreeSet<u8>) {
    let table = vlan_table_read(cfg);
    let template = table
        .entries_iter()
        .filter(|(_, n)| syscfg_get(cfg, &format!("switch.vlan.{n}.mode")) == Some("tagged"))
        .map(|(_, n)| n)
        .next_back();
    let mut fields: Vec<(String, String)> = match template {
        Some(t) => syscfg_keys_list(cfg)
            .into_iter()
            .filter_map(|key| {
                let (index, rest) = vlan_key_split(key)?;
                if index != t || rest.starts_with("port.") {
                    return None;
                }
                Some((rest.to_string(), syscfg_get(cfg, key)?.to_string()))
            })
            .collect(),
        None => vec![
            ("id".to_string(), String::new()),
            ("mode".to_string(), "tagged".to_string()),
            ("status".to_string(), "enabled".to_string()),
        ],
    };
    for (field, value) in fields.iter_mut() {
        if field == "id" {
            *value = id.to_string();
        }
    }

    let n = vlan_index_next(cfg);
    for (field, value) in &fields {
        syscfg_set(cfg, &format!("switch.vlan.{n}.{field}"), value);
    }
    for idx in ports {
        syscfg_set(cfg, &vlan_port_key(n, *idx), "exclude");
    }
}

pub fn global_patch_apply(
    cfg: &mut SysCfg,
    overlay: &mut Overlay,
    patch: &GlobalSettingsPatch,
) -> Result<(), SettingsError> {
    if let Some(version) = patch
        .stp_version
        .as_ref()
        .filter(|v| !STP_VERSIONS.contains(&v.as_str()))
    {
        return Err(SettingsError::StpVersion(version.clone()));
    }
    if let Some(priority) = patch
        .stp_priority
        .filter(|p| p % STP_PRIORITY_STEP != 0 || *p > STP_PRIORITY_MAX)
    {
        return Err(SettingsError::StpPriority(priority));
    }
    let table = vlan_table_read(cfg);
    let vlan_change = match &patch.vlans {
        Some(ids) => {
            let wanted: BTreeSet<u16> = ids.iter().copied().collect();
            if let Some(id) = wanted.iter().find(|id| **id == 0 || **id > VLAN_ID_MAX) {
                return Err(SettingsError::VlanRange(*id));
            }
            let current: BTreeSet<u16> = table.ids_list().into_iter().collect();
            let removed: BTreeSet<u16> = current.difference(&wanted).copied().collect();
            let added: BTreeSet<u16> = wanted.difference(&current).copied().collect();
            vlans_removed_check(cfg, &table, &removed)?;
            Some((removed, added))
        }
        None => None,
    };

    if let Some(version) = &patch.stp_version {
        syscfg_set(cfg, "switch.stp.version", version);
    }
    if let Some(priority) = patch.stp_priority {
        syscfg_set(cfg, "switch.stp.priority", &priority.to_string());
    }
    if let Some(action) = patch.bpdu_action {
        overlay.bpdu_action = action;
    }
    if let Some((removed, added)) = vlan_change {
        let mut removed_indices: Vec<u32> = removed
            .iter()
            .filter_map(|id| table.index_get(*id))
            .collect();
        removed_indices.sort_unstable_by(|a, b| b.cmp(a));
        for n in removed_indices {
            vlan_remove(cfg, n);
        }
        let table_now = vlan_table_read(cfg);
        let ports: BTreeSet<u8> = ports_list(cfg)
            .into_iter()
            .filter(|idx| !port_vlan_all_get(cfg, &table_now, *idx))
            .collect();
        for id in added {
            vlan_add(cfg, id, &ports);
        }
    }
    Ok(())
}
