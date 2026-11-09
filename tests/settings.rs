// Integration tests for the map between port and global settings and system.cfg keys.

use uswctl::model::Model;
use uswctl::overlay::Overlay;
use uswctl::settings::{
    BpduAction, EdgeMode, GlobalSettingsPatch, PoeMode, PortSettingsPatch, SettingsError,
    global_patch_apply, global_settings_read, port_patch_apply, port_settings_read,
    vlan_table_read,
};
use uswctl::syscfg::{SysCfg, syscfg_get, syscfg_keys_list, syscfg_parse, syscfg_render};

const XG10: &str = include_str!("fixtures/xg10/system.cfg");
const PROMAX16: &str = include_str!("fixtures/promax16/system.cfg");

fn xg() -> SysCfg {
    syscfg_parse(XG10)
}

fn promax() -> SysCfg {
    syscfg_parse(PROMAX16)
}

fn vlan_index_of(cfg: &SysCfg, id: u16) -> u32 {
    vlan_table_read(cfg)
        .index_get(id)
        .unwrap_or_else(|| panic!("VLAN {id} not in the table"))
}

fn port_mode_get(cfg: &SysCfg, vlan_id: u16, port: u8) -> Option<String> {
    let n = vlan_index_of(cfg, vlan_id);
    syscfg_get(cfg, &format!("switch.vlan.{n}.port.{port}.mode")).map(str::to_string)
}

#[test]
fn read_xg_port2() {
    let s = port_settings_read(&xg(), &Overlay::default(), 2);
    assert_eq!(s.name, "Port 2");
    assert_eq!(s.native_vlan, Some(4));
    assert!(s.tagged_vlans.is_empty());
    assert_eq!(s.poe, PoeMode::Auto);
    assert!(s.stp);
    assert_eq!(s.edge, EdgeMode::Auto);
    assert!(!s.bpdu_guard);
    assert!(!s.bpdu_filter);
    assert!(s.enabled);
}

#[test]
fn read_promax_port16() {
    let s = port_settings_read(&promax(), &Overlay::default(), 16);
    assert_eq!(s.name, "Host");
    assert_eq!(s.native_vlan, Some(1));
    assert_eq!(s.tagged_vlans, vec![2, 3, 4, 5, 6, 7, 8]);
    assert!(!s.stp);
    assert_eq!(s.poe, PoeMode::Off);
}

#[test]
fn read_promax_port2_without_edge_keys() {
    let s = port_settings_read(&promax(), &Overlay::default(), 2);
    assert_eq!(s.native_vlan, Some(2));
    assert!(s.tagged_vlans.is_empty());
    assert_eq!(s.edge, EdgeMode::Auto);
    assert!(!s.bpdu_guard);
}

#[test]
fn read_promax_port8_partial_trunk() {
    let s = port_settings_read(&promax(), &Overlay::default(), 8);
    assert_eq!(s.native_vlan, Some(1));
    assert_eq!(s.tagged_vlans, vec![2, 3, 4]);
}

#[test]
fn read_disabled_and_bpdu_filter() {
    let mut cfg = xg();
    let mut overlay = Overlay::default();
    overlay.port_bpdu_filter.insert(2);
    let patch = PortSettingsPatch {
        enabled: Some(false),
        ..Default::default()
    };
    port_patch_apply(&mut cfg, &mut Overlay::default(), Model::Xg10, 2, &patch).unwrap();
    let s = port_settings_read(&cfg, &overlay, 2);
    assert!(!s.enabled);
    assert!(s.bpdu_filter);
}

#[test]
fn patch_native_vlan() {
    let mut cfg = xg();
    let patch = PortSettingsPatch {
        native_vlan: Some(Some(3)),
        ..Default::default()
    };
    port_patch_apply(&mut cfg, &mut Overlay::default(), Model::Xg10, 2, &patch).unwrap();
    assert_eq!(syscfg_get(&cfg, "switch.port.2.pvid"), Some("3"));
    assert_eq!(port_mode_get(&cfg, 3, 2).as_deref(), Some("untagged"));
    assert_eq!(port_mode_get(&cfg, 4, 2).as_deref(), Some("exclude"));
    for vlan in [1, 2, 5, 6, 7, 8] {
        assert_eq!(port_mode_get(&cfg, vlan, 2).as_deref(), Some("exclude"));
    }
}

#[test]
fn patch_native_vlan_changes_only_port_keys() {
    let mut cfg = xg();
    let patch = PortSettingsPatch {
        native_vlan: Some(Some(3)),
        ..Default::default()
    };
    port_patch_apply(&mut cfg, &mut Overlay::default(), Model::Xg10, 2, &patch).unwrap();
    let changes = uswctl::syscfg::syscfg_diff(&xg(), &cfg);
    let keys: Vec<&str> = changes.iter().map(|c| c.key.as_str()).collect();
    assert_eq!(
        keys,
        vec![
            "switch.port.2.pvid",
            "switch.vlan.3.port.2.mode",
            "switch.vlan.4.port.2.mode"
        ]
    );
}

#[test]
fn patch_tagged_vlans_keeps_native() {
    let mut cfg = xg();
    let patch = PortSettingsPatch {
        tagged_vlans: Some(vec![5, 6]),
        ..Default::default()
    };
    port_patch_apply(&mut cfg, &mut Overlay::default(), Model::Xg10, 2, &patch).unwrap();
    let s = port_settings_read(&cfg, &Overlay::default(), 2);
    assert_eq!(s.native_vlan, Some(4));
    assert_eq!(s.tagged_vlans, vec![5, 6]);
    assert_eq!(port_mode_get(&cfg, 5, 2).as_deref(), Some("tagged"));
    assert_eq!(port_mode_get(&cfg, 1, 2).as_deref(), Some("exclude"));
}

#[test]
fn patch_native_none_removes_pvid() {
    let mut cfg = xg();
    let patch = PortSettingsPatch {
        native_vlan: Some(None),
        tagged_vlans: Some(vec![4]),
        ..Default::default()
    };
    port_patch_apply(&mut cfg, &mut Overlay::default(), Model::Xg10, 2, &patch).unwrap();
    assert_eq!(syscfg_get(&cfg, "switch.port.2.pvid"), None);
    let s = port_settings_read(&cfg, &Overlay::default(), 2);
    assert_eq!(s.native_vlan, None);
    assert_eq!(s.tagged_vlans, vec![4]);
}

#[test]
fn patch_promax_port16_writes_every_vlan() {
    let mut cfg = promax();
    let patch = PortSettingsPatch {
        tagged_vlans: Some(vec![2, 3]),
        ..Default::default()
    };
    port_patch_apply(
        &mut cfg,
        &mut Overlay::default(),
        Model::ProMax16,
        16,
        &patch,
    )
    .unwrap();
    assert_eq!(syscfg_get(&cfg, "switch.port.16.pvid"), Some("1"));
    assert_eq!(port_mode_get(&cfg, 1, 16).as_deref(), Some("untagged"));
    assert_eq!(port_mode_get(&cfg, 2, 16).as_deref(), Some("tagged"));
    assert_eq!(port_mode_get(&cfg, 3, 16).as_deref(), Some("tagged"));
    for vlan in 4..=8 {
        assert_eq!(port_mode_get(&cfg, vlan, 16).as_deref(), Some("exclude"));
    }
}

#[test]
fn patch_poe_off() {
    let mut cfg = xg();
    let patch = PortSettingsPatch {
        poe: Some(PoeMode::Off),
        ..Default::default()
    };
    port_patch_apply(&mut cfg, &mut Overlay::default(), Model::Xg10, 2, &patch).unwrap();
    assert_eq!(syscfg_get(&cfg, "switch.port.2.poe"), Some("shutdown"));
}

#[test]
fn patch_stp_edge_guard_name() {
    let mut cfg = xg();
    let patch = PortSettingsPatch {
        name: Some("DUT A".to_string()),
        stp: Some(false),
        edge: Some(EdgeMode::Enabled),
        bpdu_guard: Some(true),
        ..Default::default()
    };
    port_patch_apply(&mut cfg, &mut Overlay::default(), Model::Xg10, 2, &patch).unwrap();
    assert_eq!(syscfg_get(&cfg, "switch.port.2.name"), Some("DUT A"));
    assert_eq!(
        syscfg_get(&cfg, "switch.port.2.stp.port_mode"),
        Some("disabled")
    );
    assert_eq!(syscfg_get(&cfg, "switch.port.2.stp.edge"), Some("enabled"));
    assert_eq!(
        syscfg_get(&cfg, "switch.port.2.stp.bpdu_guard"),
        Some("enabled")
    );
}

#[test]
fn patch_without_edge_does_not_add_edge_key() {
    let mut cfg = promax();
    let patch = PortSettingsPatch {
        stp: Some(true),
        ..Default::default()
    };
    port_patch_apply(
        &mut cfg,
        &mut Overlay::default(),
        Model::ProMax16,
        2,
        &patch,
    )
    .unwrap();
    assert_eq!(syscfg_get(&cfg, "switch.port.2.stp.edge"), None);
    assert_eq!(syscfg_get(&cfg, "switch.port.2.stp.bpdu_guard"), None);
    assert_eq!(
        syscfg_get(&cfg, "switch.port.2.stp.port_mode"),
        Some("enabled")
    );
}

#[test]
fn patch_enabled_toggles_status_key() {
    let mut cfg = xg();
    let off = PortSettingsPatch {
        enabled: Some(false),
        ..Default::default()
    };
    port_patch_apply(&mut cfg, &mut Overlay::default(), Model::Xg10, 2, &off).unwrap();
    assert_eq!(syscfg_get(&cfg, "switch.port.2.status"), Some("disabled"));

    let on = PortSettingsPatch {
        enabled: Some(true),
        ..Default::default()
    };
    port_patch_apply(&mut cfg, &mut Overlay::default(), Model::Xg10, 2, &on).unwrap();
    assert_eq!(syscfg_get(&cfg, "switch.port.2.status"), None);
    assert_eq!(syscfg_render(&cfg), XG10);
}

#[test]
fn patch_bpdu_filter_sets_overlay() {
    let mut cfg = xg();
    let mut overlay = Overlay::default();
    let on = PortSettingsPatch {
        bpdu_filter: Some(true),
        ..Default::default()
    };
    port_patch_apply(&mut cfg, &mut overlay, Model::Xg10, 2, &on).unwrap();
    assert!(overlay.port_bpdu_filter.contains(&2));
    assert_eq!(syscfg_render(&cfg), XG10);

    let off = PortSettingsPatch {
        bpdu_filter: Some(false),
        ..Default::default()
    };
    port_patch_apply(&mut cfg, &mut overlay, Model::Xg10, 2, &off).unwrap();
    assert!(!overlay.port_bpdu_filter.contains(&2));
}

#[test]
fn patch_rejects_port_0() {
    let mut cfg = xg();
    let patch = PortSettingsPatch::default();
    let err = port_patch_apply(&mut cfg, &mut Overlay::default(), Model::Xg10, 0, &patch);
    assert_eq!(err, Err(SettingsError::PortRange(0)));
}

#[test]
fn patch_rejects_port_13_on_xg() {
    let mut cfg = xg();
    let patch = PortSettingsPatch::default();
    let err = port_patch_apply(&mut cfg, &mut Overlay::default(), Model::Xg10, 13, &patch);
    assert_eq!(err, Err(SettingsError::PortRange(13)));
}

#[test]
fn patch_rejects_unknown_vlan() {
    let mut cfg = xg();
    let mut overlay = Overlay::default();
    let patch = PortSettingsPatch {
        tagged_vlans: Some(vec![5, 99]),
        poe: Some(PoeMode::Off),
        bpdu_filter: Some(true),
        ..Default::default()
    };
    let err = port_patch_apply(&mut cfg, &mut overlay, Model::Xg10, 2, &patch);
    assert_eq!(err, Err(SettingsError::VlanUnknown(99)));
    assert_eq!(syscfg_render(&cfg), XG10);
    assert_eq!(overlay, Overlay::default());
}

#[test]
fn patch_rejects_unknown_native_vlan() {
    let mut cfg = xg();
    let patch = PortSettingsPatch {
        native_vlan: Some(Some(99)),
        ..Default::default()
    };
    let err = port_patch_apply(&mut cfg, &mut Overlay::default(), Model::Xg10, 2, &patch);
    assert_eq!(err, Err(SettingsError::VlanUnknown(99)));
}

#[test]
fn patch_rejects_native_in_tagged() {
    let mut cfg = xg();
    let patch = PortSettingsPatch {
        native_vlan: Some(Some(3)),
        tagged_vlans: Some(vec![3, 5]),
        ..Default::default()
    };
    let err = port_patch_apply(&mut cfg, &mut Overlay::default(), Model::Xg10, 2, &patch);
    assert_eq!(err, Err(SettingsError::NativeAlsoTagged(3)));
    assert_eq!(syscfg_render(&cfg), XG10);
}

#[test]
fn patch_rejects_tagged_that_hits_current_native() {
    let mut cfg = xg();
    let patch = PortSettingsPatch {
        tagged_vlans: Some(vec![4]),
        ..Default::default()
    };
    let err = port_patch_apply(&mut cfg, &mut Overlay::default(), Model::Xg10, 2, &patch);
    assert_eq!(err, Err(SettingsError::NativeAlsoTagged(4)));
}

#[test]
fn patch_rejects_name_with_newline() {
    let mut cfg = xg();
    let patch = PortSettingsPatch {
        name: Some("x\nswitch.port.11.status=disabled".to_string()),
        ..Default::default()
    };
    let err = port_patch_apply(&mut cfg, &mut Overlay::default(), Model::Xg10, 2, &patch);
    assert!(matches!(err, Err(SettingsError::PortName(_))));
    assert_eq!(syscfg_render(&cfg), XG10);
}

#[test]
fn patch_rejects_name_with_quote_backslash_or_control() {
    for name in [
        "DUT \"3\"",
        "DUT\\3",
        "DUT\t3",
        "DUT\u{7f}3",
        "DUT\u{1b}[2K",
    ] {
        let mut cfg = xg();
        let patch = PortSettingsPatch {
            name: Some(name.to_string()),
            ..Default::default()
        };
        let err = port_patch_apply(&mut cfg, &mut Overlay::default(), Model::Xg10, 2, &patch);
        assert!(matches!(err, Err(SettingsError::PortName(_))), "{name:?}");
        assert_eq!(syscfg_render(&cfg), XG10, "{name:?}");
    }
    let mut cfg = xg();
    let patch = PortSettingsPatch {
        name: Some("DUT+3 (lab) #1".to_string()),
        ..Default::default()
    };
    port_patch_apply(&mut cfg, &mut Overlay::default(), Model::Xg10, 2, &patch).unwrap();
    assert_eq!(
        syscfg_get(&cfg, "switch.port.2.name"),
        Some("DUT+3 (lab) #1")
    );
}

#[test]
fn patch_json_null_native_clears() {
    let absent: PortSettingsPatch = serde_json::from_str("{}").unwrap();
    assert_eq!(absent.native_vlan, None);
    let null: PortSettingsPatch = serde_json::from_str(r#"{"native_vlan":null}"#).unwrap();
    assert_eq!(null.native_vlan, Some(None));
    let set: PortSettingsPatch =
        serde_json::from_str(r#"{"native_vlan":3,"poe":"off","edge":"auto"}"#).unwrap();
    assert_eq!(set.native_vlan, Some(Some(3)));
    assert_eq!(set.poe, Some(PoeMode::Off));
    assert_eq!(set.edge, Some(EdgeMode::Auto));
}

#[test]
fn patch_json_unknown_field_refused() {
    assert!(serde_json::from_str::<PortSettingsPatch>(r#"{"nmae":"typo"}"#).is_err());
    assert!(serde_json::from_str::<GlobalSettingsPatch>(r#"{"stp_prio":4096}"#).is_err());
}

#[test]
fn global_read_xg() {
    let g = global_settings_read(&xg(), &Overlay::default());
    assert_eq!(g.stp_version, "rstp");
    assert_eq!(g.stp_priority, 32768);
    assert_eq!(g.bpdu_action, BpduAction::Flooding);
    assert_eq!(g.vlans, vec![1, 2, 3, 4, 5, 6, 7, 8]);
}

#[test]
fn global_add_vlan_9() {
    let mut cfg = xg();
    let patch = GlobalSettingsPatch {
        vlans: Some((1..=9).collect()),
        ..Default::default()
    };
    global_patch_apply(&mut cfg, &mut Overlay::default(), &patch).unwrap();
    let n = vlan_index_of(&cfg, 9);
    assert_eq!(syscfg_get(&cfg, &format!("switch.vlan.{n}.id")), Some("9"));
    assert_eq!(
        syscfg_get(&cfg, &format!("switch.vlan.{n}.mode")),
        Some("tagged")
    );
    assert_eq!(
        syscfg_get(&cfg, &format!("switch.vlan.{n}.status")),
        Some("enabled")
    );
    for port in 1..=6 {
        assert_eq!(port_mode_get(&cfg, 9, port).as_deref(), Some("exclude"));
    }
    for port in 7..=12 {
        assert_eq!(port_mode_get(&cfg, 9, port), None);
    }
    let g = global_settings_read(&cfg, &Overlay::default());
    assert_eq!(g.vlans, (1..=9).collect::<Vec<u16>>());
    let s = port_settings_read(&cfg, &Overlay::default(), 2);
    assert_eq!(s.native_vlan, Some(4));
    assert!(s.tagged_vlans.is_empty());
}

#[test]
fn global_add_vlan_9_keeps_promax_trunk() {
    let mut cfg = promax();
    let patch = GlobalSettingsPatch {
        vlans: Some((1..=9).collect()),
        ..Default::default()
    };
    global_patch_apply(&mut cfg, &mut Overlay::default(), &patch).unwrap();
    assert_eq!(port_mode_get(&cfg, 9, 2).as_deref(), Some("exclude"));
    for port in [12, 16, 17] {
        assert_eq!(port_mode_get(&cfg, 9, port), None);
    }
    let s = port_settings_read(&cfg, &Overlay::default(), 16);
    assert_eq!(s.native_vlan, Some(1));
    assert_eq!(s.tagged_vlans, vec![2, 3, 4, 5, 6, 7, 8, 9]);
    assert!(s.vlan_all);
}

#[test]
fn global_add_vlan_9_keeps_xg_port11_all() {
    let mut cfg = xg();
    let patch = GlobalSettingsPatch {
        vlans: Some((1..=9).collect()),
        ..Default::default()
    };
    global_patch_apply(&mut cfg, &mut Overlay::default(), &patch).unwrap();
    assert_eq!(port_mode_get(&cfg, 9, 11), None);
    assert!(port_settings_read(&cfg, &Overlay::default(), 11).vlan_all);
}

#[test]
fn read_vlan_all() {
    assert!(port_settings_read(&promax(), &Overlay::default(), 16).vlan_all);
    assert!(!port_settings_read(&xg(), &Overlay::default(), 2).vlan_all);
}

#[test]
fn patch_tagged_on_vlan_all_port_makes_list_explicit() {
    let mut cfg = promax();
    let patch = PortSettingsPatch {
        tagged_vlans: Some(vec![2, 3]),
        ..Default::default()
    };
    port_patch_apply(
        &mut cfg,
        &mut Overlay::default(),
        Model::ProMax16,
        16,
        &patch,
    )
    .unwrap();
    let s = port_settings_read(&cfg, &Overlay::default(), 16);
    assert!(!s.vlan_all);
    assert_eq!(s.native_vlan, Some(1));
    assert_eq!(s.tagged_vlans, vec![2, 3]);
}

#[test]
fn global_remove_vlan_5_renumbers() {
    let original = promax();
    let mut cfg = promax();
    // Pro Max port 5 has native VLAN 5, and a VLAN in use as native cannot go.
    let move_port5 = PortSettingsPatch {
        native_vlan: Some(Some(2)),
        ..Default::default()
    };
    port_patch_apply(
        &mut cfg,
        &mut Overlay::default(),
        Model::ProMax16,
        5,
        &move_port5,
    )
    .unwrap();
    assert_eq!(
        global_patch_apply(
            &mut promax(),
            &mut Overlay::default(),
            &GlobalSettingsPatch {
                vlans: Some(vec![1, 2, 3, 4, 6, 7, 8]),
                ..Default::default()
            }
        ),
        Err(SettingsError::VlanInUse(5))
    );
    let patch = GlobalSettingsPatch {
        vlans: Some(vec![1, 2, 3, 4, 6, 7, 8]),
        ..Default::default()
    };
    global_patch_apply(&mut cfg, &mut Overlay::default(), &patch).unwrap();
    let table = vlan_table_read(&cfg);
    let mut indices: Vec<u32> = table
        .ids_list()
        .into_iter()
        .map(|id| table.index_get(id).unwrap())
        .collect();
    indices.sort();
    assert_eq!(indices, (1..=7).collect::<Vec<u32>>());
    for (id, n) in [(6u16, 5u32), (7, 6), (8, 7)] {
        assert_eq!(table.index_get(id), Some(n));
        for port in 1..=18u8 {
            let old = syscfg_get(&original, &format!("switch.vlan.{id}.port.{port}.mode"));
            let new = syscfg_get(&cfg, &format!("switch.vlan.{n}.port.{port}.mode"));
            assert_eq!(old, new, "VLAN {id} port {port}");
        }
        assert_eq!(
            syscfg_get(&cfg, &format!("switch.vlan.{n}.status")),
            Some("enabled")
        );
    }
    assert!(
        syscfg_keys_list(&cfg)
            .iter()
            .all(|k| !k.starts_with("switch.vlan.8."))
    );
    assert_eq!(
        port_settings_read(&cfg, &Overlay::default(), 15).native_vlan,
        Some(8)
    );
    assert_eq!(
        port_settings_read(&cfg, &Overlay::default(), 16).tagged_vlans,
        vec![2, 3, 4, 6, 7, 8]
    );
}

#[test]
fn global_rejects_removal_of_management_vlan() {
    for text in [XG10, PROMAX16] {
        let mut cfg = syscfg_parse(text);
        let patch = GlobalSettingsPatch {
            vlans: Some((2..=8).collect()),
            ..Default::default()
        };
        let err = global_patch_apply(&mut cfg, &mut Overlay::default(), &patch);
        assert_eq!(err, Err(SettingsError::VlanManagement(1)));
        assert_eq!(syscfg_render(&cfg), text);
    }
}

#[test]
fn patch_rejects_poe_on_port_without_poe() {
    let mut cfg = xg();
    let patch = PortSettingsPatch {
        poe: Some(PoeMode::Off),
        name: Some("x".to_string()),
        ..Default::default()
    };
    let err = port_patch_apply(&mut cfg, &mut Overlay::default(), Model::Xg10, 11, &patch);
    assert_eq!(err, Err(SettingsError::PoeUnsupported(11)));
    assert_eq!(syscfg_render(&cfg), XG10);
}

#[test]
fn global_remove_vlan_8() {
    let mut cfg = xg();
    let patch = GlobalSettingsPatch {
        vlans: Some((1..=7).collect()),
        ..Default::default()
    };
    global_patch_apply(&mut cfg, &mut Overlay::default(), &patch).unwrap();
    assert!(
        syscfg_keys_list(&cfg)
            .iter()
            .all(|k| !k.starts_with("switch.vlan.8."))
    );
    let g = global_settings_read(&cfg, &Overlay::default());
    assert_eq!(g.vlans, (1..=7).collect::<Vec<u16>>());
}

#[test]
fn global_rejects_removal_of_native_vlan() {
    let mut cfg = xg();
    let patch = GlobalSettingsPatch {
        vlans: Some(vec![1, 2, 3, 5, 6, 7, 8]),
        ..Default::default()
    };
    let err = global_patch_apply(&mut cfg, &mut Overlay::default(), &patch);
    assert_eq!(err, Err(SettingsError::VlanInUse(4)));
    assert_eq!(syscfg_render(&cfg), XG10);
}

#[test]
fn global_rejects_vlan_out_of_range() {
    let mut cfg = xg();
    let mut ids: Vec<u16> = (1..=8).collect();
    ids.push(4095);
    let patch = GlobalSettingsPatch {
        vlans: Some(ids),
        ..Default::default()
    };
    let err = global_patch_apply(&mut cfg, &mut Overlay::default(), &patch);
    assert_eq!(err, Err(SettingsError::VlanRange(4095)));
}

#[test]
fn global_stp_keys() {
    let mut cfg = xg();
    let patch = GlobalSettingsPatch {
        stp_version: Some("stp".to_string()),
        stp_priority: Some(4096),
        ..Default::default()
    };
    global_patch_apply(&mut cfg, &mut Overlay::default(), &patch).unwrap();
    assert_eq!(syscfg_get(&cfg, "switch.stp.version"), Some("stp"));
    assert_eq!(syscfg_get(&cfg, "switch.stp.priority"), Some("4096"));
}

#[test]
fn global_rejects_bad_stp_values() {
    let mut cfg = xg();
    let version = GlobalSettingsPatch {
        stp_version: Some("pvst".to_string()),
        ..Default::default()
    };
    assert!(matches!(
        global_patch_apply(&mut cfg, &mut Overlay::default(), &version),
        Err(SettingsError::StpVersion(_))
    ));
    let priority = GlobalSettingsPatch {
        stp_priority: Some(1000),
        ..Default::default()
    };
    assert_eq!(
        global_patch_apply(&mut cfg, &mut Overlay::default(), &priority),
        Err(SettingsError::StpPriority(1000))
    );
    assert_eq!(syscfg_render(&cfg), XG10);
}

#[test]
fn global_bpdu_filtering_sets_overlay() {
    let mut cfg = xg();
    let mut overlay = Overlay::default();
    let patch = GlobalSettingsPatch {
        bpdu_action: Some(BpduAction::Filtering),
        ..Default::default()
    };
    global_patch_apply(&mut cfg, &mut overlay, &patch).unwrap();
    assert_eq!(overlay.bpdu_action, BpduAction::Filtering);
    assert_eq!(syscfg_render(&cfg), XG10);
    let g = global_settings_read(&cfg, &overlay);
    assert_eq!(g.bpdu_action, BpduAction::Filtering);
}
