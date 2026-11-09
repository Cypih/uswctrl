// Read the switch's running configuration and check it against an overlay.

use crate::model::{Model, model_cli_port_name, model_port_count};
use crate::overlay::Overlay;
use crate::settings::BpduAction;

// `cli` without `terminal length 0` interleaves `--More--` and cursor-redraw escapes into
// the capture. Both are terminal noise, never part of the configuration text.
fn line_strip_pager_noise(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        if c == '\u{8}' {
            continue;
        }
        out.push(c);
    }
    out.replace("--More--", "")
}

pub fn running_lines_clean(running: &str) -> Vec<String> {
    running
        .lines()
        .map(line_strip_pager_noise)
        .filter(|line| !line.is_empty())
        .collect()
}

pub fn runcfg_port_block_get(running: &str, cli_name: &str) -> Vec<String> {
    let header = format!("interface {cli_name}");
    let lines = running_lines_clean(running);
    let Some(start) = lines.iter().position(|line| line == &header) else {
        return Vec::new();
    };
    lines[start + 1..]
        .iter()
        .take_while(|line| line.as_str() != "!" && !line.starts_with("interface "))
        .cloned()
        .collect()
}

pub fn runcfg_overlay_check(running: &str, overlay: &Overlay, model: Model) -> bool {
    let lines = running_lines_clean(running);
    let global_filtering = lines
        .iter()
        .any(|line| line == "spanning-tree bpdu filtering");
    let global_ok = match overlay.bpdu_action {
        BpduAction::Filtering => global_filtering,
        BpduAction::Flooding => !global_filtering,
    };
    if !global_ok {
        return false;
    }
    (1..=model_port_count(model)).all(|idx| {
        let Some(name) = model_cli_port_name(model, idx) else {
            return true;
        };
        let has_filter = runcfg_port_block_get(running, &name)
            .iter()
            .any(|line| line == " spanning-tree bpdu-filter");
        has_filter == overlay.port_bpdu_filter.contains(&idx)
    })
}
