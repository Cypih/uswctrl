// Integration tests for system.cfg parse, change and render.

use uswctl::syscfg::{
    KeyChange, syscfg_diff, syscfg_get, syscfg_keys_list, syscfg_parse, syscfg_remove,
    syscfg_rename, syscfg_render, syscfg_set,
};

const XG10: &str = include_str!("fixtures/xg10/system.cfg");
const PROMAX16: &str = include_str!("fixtures/promax16/system.cfg");

#[test]
fn render_is_identity_xg10() {
    let cfg = syscfg_parse(XG10);
    assert_eq!(syscfg_render(&cfg), XG10);
}

#[test]
fn render_is_identity_promax16() {
    let cfg = syscfg_parse(PROMAX16);
    assert_eq!(syscfg_render(&cfg), PROMAX16);
}

#[test]
fn odd_lines_survive() {
    let text = "# c\n\nnoequals\na=1\na=2\n";
    let cfg = syscfg_parse(text);
    assert_eq!(syscfg_render(&cfg), text);
    assert_eq!(syscfg_get(&cfg, "a"), Some("2"));
}

#[test]
fn set_existing() {
    let mut cfg = syscfg_parse(XG10);
    syscfg_set(&mut cfg, "switch.port.2.name", "x");
    let rendered = syscfg_render(&cfg);
    assert_eq!(syscfg_get(&cfg, "switch.port.2.name"), Some("x"));

    let before: Vec<&str> = XG10.lines().collect();
    let after: Vec<&str> = rendered.lines().collect();
    assert_eq!(before.len(), after.len());
    let changed: Vec<(usize, (&&str, &&str))> = before
        .iter()
        .zip(after.iter())
        .enumerate()
        .filter(|(_, (b, a))| b != a)
        .collect();
    assert_eq!(changed.len(), 1);
    assert_eq!(*changed[0].1.1, "switch.port.2.name=x");
}

#[test]
fn set_new_key_groups() {
    let mut cfg = syscfg_parse(XG10);
    syscfg_set(&mut cfg, "switch.port.2.stp.root_guard", "enabled");
    let rendered = syscfg_render(&cfg);
    let lines: Vec<&str> = rendered.lines().collect();
    let bpdu_guard_idx = lines
        .iter()
        .position(|l| *l == "switch.port.2.stp.bpdu_guard=disabled")
        .unwrap();
    assert_eq!(
        lines[bpdu_guard_idx + 1],
        "switch.port.2.stp.root_guard=enabled"
    );
}

#[test]
fn set_duplicate_key() {
    let text = "# c\n\nnoequals\na=1\na=2\n";
    let mut cfg = syscfg_parse(text);
    syscfg_set(&mut cfg, "a", "9");
    assert_eq!(syscfg_render(&cfg), "# c\n\nnoequals\na=9\na=9\n");
}

#[test]
fn remove_key() {
    let mut cfg = syscfg_parse(XG10);
    syscfg_remove(&mut cfg, "switch.port.2.name");
    assert_eq!(syscfg_get(&cfg, "switch.port.2.name"), None);
    assert!(!syscfg_render(&cfg).contains("switch.port.2.name="));
}

#[test]
fn diff_lists_changes() {
    let old = syscfg_parse("a=1\nb=1\nc=1\n");
    let new = syscfg_parse("b=2\nc=1\nd=1\n");
    let diff = syscfg_diff(&old, &new);
    assert_eq!(
        diff,
        vec![
            KeyChange {
                key: "b".to_string(),
                old: Some("1".to_string()),
                new: Some("2".to_string()),
            },
            KeyChange {
                key: "d".to_string(),
                old: None,
                new: Some("1".to_string()),
            },
            KeyChange {
                key: "a".to_string(),
                old: Some("1".to_string()),
                new: None,
            },
        ]
    );
}

#[test]
fn keys_list_once_in_order() {
    let cfg = syscfg_parse("# c\nb=1\na=2\nb=3\nnoequals\n");
    assert_eq!(syscfg_keys_list(&cfg), vec!["b", "a"]);
}

#[test]
fn rename_keeps_position() {
    let mut cfg = syscfg_parse("a=1\nb=2\nc=3\nb=4\n");
    syscfg_rename(&mut cfg, "b", "x");
    assert_eq!(syscfg_render(&cfg), "a=1\nx=2\nc=3\nx=4\n");
}
