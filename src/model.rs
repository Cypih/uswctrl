// Switch models, CLI port names and switch commands found by the device discovery (docs/device-notes.md).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    ProMax16,
    Xg10,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoeAction {
    Off,
    On,
    Cycle,
}

// The pager setting holds for one `cli` session only, so each `cli` call must send it first.
pub const CLI_PAGER_OFF: Option<&str> = Some("terminal length 0");

pub fn model_from_code(code: &str) -> Option<Model> {
    match code {
        "USPM16P" => Some(Model::ProMax16),
        // The controller reports "USWED77"; mca-dump on the switch itself reports
        // "USPXG10P", its own model string.
        "USWED77" | "USPXG10P" => Some(Model::Xg10),
        _ => None,
    }
}

pub fn model_port_count(model: Model) -> u8 {
    match model {
        Model::ProMax16 => 18,
        Model::Xg10 => 12,
    }
}

pub fn model_cli_port_name(model: Model, idx: u8) -> Option<String> {
    if idx == 0 || idx > model_port_count(model) {
        return None;
    }
    let name = match model {
        Model::Xg10 => format!("te{idx}"),
        Model::ProMax16 => match idx {
            1..=12 => format!("gi{idx}"),
            13..=16 => format!("tw{}", idx - 12),
            _ => format!("te{}", idx - 16),
        },
    };
    Some(name)
}

// `switch.port.N.opmode=disabled` looks like the obvious key, but `ubntconf` generates no
// command for it; only `status=disabled` gives `shutdown`.
pub fn port_disable_key(idx: u8) -> (String, String) {
    (format!("switch.port.{idx}.status"), "disabled".to_string())
}

// `swctrl` changes only the live PoE state and leaves `system.cfg` as it is, which keeps
// the action transient.
pub fn poe_action_script(idx: u8, action: PoeAction) -> String {
    let command = match action {
        PoeAction::Off => "set off",
        PoeAction::On => "set auto",
        PoeAction::Cycle => "restart",
    };
    format!("swctrl poe {command} id {idx}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_from_code_known() {
        assert_eq!(model_from_code("USPM16P"), Some(Model::ProMax16));
        assert_eq!(model_from_code("USWED77"), Some(Model::Xg10));
    }

    #[test]
    fn model_from_code_xg10_mca_code() {
        assert_eq!(model_from_code("USPXG10P"), Some(Model::Xg10));
    }

    #[test]
    fn model_from_code_unknown() {
        assert_eq!(model_from_code("US8P150"), None);
        assert_eq!(model_from_code(""), None);
    }

    #[test]
    fn model_port_count_values() {
        assert_eq!(model_port_count(Model::ProMax16), 18);
        assert_eq!(model_port_count(Model::Xg10), 12);
    }

    #[test]
    fn model_cli_port_name_xg10() {
        assert_eq!(model_cli_port_name(Model::Xg10, 1).as_deref(), Some("te1"));
        assert_eq!(
            model_cli_port_name(Model::Xg10, 11).as_deref(),
            Some("te11")
        );
        assert_eq!(
            model_cli_port_name(Model::Xg10, 12).as_deref(),
            Some("te12")
        );
    }

    #[test]
    fn model_cli_port_name_promax16() {
        assert_eq!(
            model_cli_port_name(Model::ProMax16, 1).as_deref(),
            Some("gi1")
        );
        assert_eq!(
            model_cli_port_name(Model::ProMax16, 12).as_deref(),
            Some("gi12")
        );
        assert_eq!(
            model_cli_port_name(Model::ProMax16, 13).as_deref(),
            Some("tw1")
        );
        assert_eq!(
            model_cli_port_name(Model::ProMax16, 16).as_deref(),
            Some("tw4")
        );
        assert_eq!(
            model_cli_port_name(Model::ProMax16, 17).as_deref(),
            Some("te1")
        );
        assert_eq!(
            model_cli_port_name(Model::ProMax16, 18).as_deref(),
            Some("te2")
        );
    }

    #[test]
    fn model_cli_port_name_out_of_range() {
        assert_eq!(model_cli_port_name(Model::ProMax16, 19), None);
        assert_eq!(model_cli_port_name(Model::ProMax16, 0), None);
        assert_eq!(model_cli_port_name(Model::Xg10, 13), None);
        assert_eq!(model_cli_port_name(Model::Xg10, 0), None);
    }

    #[test]
    fn model_port_disable_key() {
        assert_eq!(
            port_disable_key(3),
            ("switch.port.3.status".to_string(), "disabled".to_string())
        );
    }

    #[test]
    fn model_poe_action_script() {
        assert_eq!(
            poe_action_script(1, PoeAction::Off),
            "swctrl poe set off id 1"
        );
        assert_eq!(
            poe_action_script(1, PoeAction::On),
            "swctrl poe set auto id 1"
        );
        assert_eq!(
            poe_action_script(7, PoeAction::Cycle),
            "swctrl poe restart id 7"
        );
    }

    #[test]
    fn model_cli_pager_off() {
        assert_eq!(CLI_PAGER_OFF, Some("terminal length 0"));
    }
}
