// Settings that system.cfg cannot hold, applied on the switch through a CLI overlay file.

use std::collections::{BTreeMap, BTreeSet};

use crate::model::{Model, model_cli_port_name, model_port_count};
use crate::settings::BpduAction;

#[derive(Debug, Clone, PartialEq)]
pub struct Overlay {
    pub bpdu_action: BpduAction,
    pub port_bpdu_filter: BTreeSet<u8>,
}

impl Default for Overlay {
    fn default() -> Self {
        Overlay {
            bpdu_action: BpduAction::Flooding,
            port_bpdu_filter: BTreeSet::new(),
        }
    }
}

// The render always states every port, flooding or filtering, so that applying it also
// removes a filter the overlay no longer wants.
pub fn overlay_render(overlay: &Overlay, model: Model) -> String {
    let mut text = String::new();
    text.push_str(match overlay.bpdu_action {
        BpduAction::Filtering => "spanning-tree bpdu filtering\n",
        BpduAction::Flooding => "no spanning-tree bpdu\n",
    });
    for idx in 1..=model_port_count(model) {
        let Some(name) = model_cli_port_name(model, idx) else {
            continue;
        };
        text.push_str(&format!("interface {name}\n"));
        text.push_str(if overlay.port_bpdu_filter.contains(&idx) {
            " spanning-tree bpdu-filter\n"
        } else {
            " no spanning-tree bpdu-filter\n"
        });
        text.push_str("exit\n");
    }
    text
}

pub fn overlay_parse(text: &str, model: Model) -> Overlay {
    let name_to_idx: BTreeMap<String, u8> = (1..=model_port_count(model))
        .filter_map(|idx| model_cli_port_name(model, idx).map(|name| (name, idx)))
        .collect();
    let mut overlay = Overlay::default();
    let mut current_idx: Option<u8> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "spanning-tree bpdu filtering" {
            overlay.bpdu_action = BpduAction::Filtering;
        } else if trimmed == "no spanning-tree bpdu" {
            overlay.bpdu_action = BpduAction::Flooding;
        } else if let Some(name) = trimmed.strip_prefix("interface ") {
            current_idx = name_to_idx.get(name).copied();
        } else if trimmed == "exit" {
            current_idx = None;
        } else if trimmed == "spanning-tree bpdu-filter"
            && let Some(idx) = current_idx
        {
            overlay.port_bpdu_filter.insert(idx);
        }
    }
    overlay
}
