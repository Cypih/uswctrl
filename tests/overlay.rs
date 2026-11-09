// Integration tests for the CLI overlay render/parse and the running-config checks.

use std::collections::BTreeSet;

use uswctl::model::Model;
use uswctl::overlay::{Overlay, overlay_parse, overlay_render};
use uswctl::runcfg::{runcfg_overlay_check, runcfg_port_block_get};
use uswctl::settings::BpduAction;

const XG10_RUNNING: &str = include_str!("fixtures/xg10/running-config.txt");

fn overlay_with(action: BpduAction, ports: &[u8]) -> Overlay {
    Overlay {
        bpdu_action: action,
        port_bpdu_filter: ports.iter().copied().collect::<BTreeSet<u8>>(),
    }
}

#[test]
fn render_parse_roundtrip() {
    let overlay = overlay_with(BpduAction::Filtering, &[1, 3]);
    let rendered = overlay_render(&overlay, Model::Xg10);
    let parsed = overlay_parse(&rendered, Model::Xg10);
    assert_eq!(parsed, overlay);
}

#[test]
fn render_starts_with_global() {
    let filtering = overlay_with(BpduAction::Filtering, &[]);
    let flooding = overlay_with(BpduAction::Flooding, &[]);
    assert!(overlay_render(&filtering, Model::Xg10).starts_with("spanning-tree bpdu filtering\n"));
    assert!(overlay_render(&flooding, Model::Xg10).starts_with("no spanning-tree bpdu\n"));
}

#[test]
fn render_covers_all_ports() {
    let overlay = Overlay::default();
    let rendered = overlay_render(&overlay, Model::Xg10);
    let block_count = rendered.matches("interface te").count();
    assert_eq!(block_count, 12);
}

#[test]
fn port_block_strips_pager() {
    let block = runcfg_port_block_get(XG10_RUNNING, "te2");
    assert!(!block.is_empty());
    for line in &block {
        assert!(
            !line.contains("--More--"),
            "line kept the pager marker: {line:?}"
        );
        assert!(
            !line.contains('\u{1b}'),
            "line kept an escape sequence: {line:?}"
        );
    }
    assert!(block.contains(&" switchport hybrid pvid 4".to_string()));
}

#[test]
fn overlay_check_false_without_global_line() {
    let overlay = overlay_with(BpduAction::Filtering, &[]);
    assert!(!runcfg_overlay_check(XG10_RUNNING, &overlay, Model::Xg10));
}

#[test]
fn overlay_check_true_for_default() {
    let overlay = Overlay::default();
    assert!(runcfg_overlay_check(XG10_RUNNING, &overlay, Model::Xg10));
}
