// Port locate: the port bitmap and the switch commands that make chosen port LEDs blink.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::model::{Model, model_port_count};
use crate::settings::SettingsError;

pub const LOCATE_DURATION_DEFAULT: Duration = Duration::from_secs(300);
pub const LOCATE_DURATION_MIN_S: u64 = 10;
pub const LOCATE_DURATION_MAX_S: u64 = 3600;
// A stop sends this command also when the daemon knows no locate, so that a stop can end a
// locate that started before a daemon restart.
pub const LOCATE_OFF_SCRIPT: &str = "syswrapper.sh unset-port-locate";

const LOCATE_WORD_BITS: u8 = 32;

/// The two words of the LED port bitmap: port N is bit N-1, ports 1 to 32 are in the low
/// word and ports 33 to 64 in the high word. The caller checks the ports first.
pub fn locate_bitmap_build(ports: &[u8]) -> (u32, u32) {
    let mut words = (0u32, 0u32);
    for &port in ports {
        let Some(bit) = port.checked_sub(1) else {
            continue;
        };
        if bit < LOCATE_WORD_BITS {
            words.1 |= 1 << bit;
        } else if bit < 2 * LOCATE_WORD_BITS {
            words.0 |= 1 << (bit - LOCATE_WORD_BITS);
        }
    }
    words
}

/// The command that makes `ports` blink: `syswrapper.sh set-port-locate <hi> <lo>`, both
/// words in lowercase hexadecimal without `0x`.
pub fn locate_script_build(ports: &[u8]) -> String {
    let (hi, lo) = locate_bitmap_build(ports);
    format!("syswrapper.sh set-port-locate {hi:x} {lo:x}")
}

/// Checks that `ports` names at least one port, each port once, and only ports of `model`.
/// Returns the ports in ascending order.
pub fn locate_ports_check(model: Model, ports: &[u8]) -> Result<Vec<u8>, SettingsError> {
    if ports.is_empty() {
        return Err(SettingsError::PortsEmpty);
    }
    let count = model_port_count(model);
    let mut seen = BTreeSet::new();
    for &port in ports {
        if port == 0 || port > count {
            return Err(SettingsError::PortRange(port));
        }
        if !seen.insert(port) {
            return Err(SettingsError::PortDuplicate(port));
        }
    }
    Ok(seen.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_bitmap_single_ports() {
        assert_eq!(locate_bitmap_build(&[1]), (0, 0x1));
        assert_eq!(locate_bitmap_build(&[2]), (0, 0x2));
        assert_eq!(locate_bitmap_build(&[32]), (0, 0x8000_0000));
        assert_eq!(locate_bitmap_build(&[33]), (0x1, 0));
        assert_eq!(locate_bitmap_build(&[52]), (0x8_0000, 0));
    }

    #[test]
    fn locate_bitmap_mix() {
        assert_eq!(locate_bitmap_build(&[1, 2]), (0, 0x3));
        assert_eq!(
            locate_bitmap_build(&[52, 1, 33, 32, 2]),
            (0x8_0001, 0x8000_0003)
        );
    }

    #[test]
    fn locate_script_text() {
        assert_eq!(
            locate_script_build(&[1, 2]),
            "syswrapper.sh set-port-locate 0 3"
        );
        assert_eq!(
            locate_script_build(&[12, 32]),
            "syswrapper.sh set-port-locate 0 80000800"
        );
        assert_eq!(
            locate_script_build(&[33, 52]),
            "syswrapper.sh set-port-locate 80001 0"
        );
        assert_eq!(LOCATE_OFF_SCRIPT, "syswrapper.sh unset-port-locate");
    }

    #[test]
    fn locate_ports_check_rules() {
        assert_eq!(
            locate_ports_check(Model::Xg10, &[12, 1, 3]),
            Ok(vec![1, 3, 12])
        );
        assert_eq!(locate_ports_check(Model::ProMax16, &[18]), Ok(vec![18]));
        assert_eq!(
            locate_ports_check(Model::Xg10, &[13]),
            Err(SettingsError::PortRange(13))
        );
        assert_eq!(
            locate_ports_check(Model::Xg10, &[0]),
            Err(SettingsError::PortRange(0))
        );
        assert_eq!(
            locate_ports_check(Model::Xg10, &[2, 2]),
            Err(SettingsError::PortDuplicate(2))
        );
        assert_eq!(
            locate_ports_check(Model::Xg10, &[]),
            Err(SettingsError::PortsEmpty)
        );
    }
}
