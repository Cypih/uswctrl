// Integration tests for the mca-dump port parser and the health probe parser.

use uswctl::live::{mca_ports_parse, probe_parse, probe_script_build};

const XG10_MCA: &str = include_str!("fixtures/xg10/mca-dump.json");
const XG10_PROBE: &str = include_str!("fixtures/xg10/probe.txt");

const MISSING_CFGMD5: &str = "\
@@version
US3.7.5.15
@@uptime
123.4 55.6
@@mem
MemAvailable:    1000 kB
@@offline
@@inform
Status:      Connected
@@lock
@@mca
{\"version\":\"1.0\",\"model\":\"X\",\"port_table\":[]}
";

#[test]
fn mca_xg_port1_poe() {
    let ports = mca_ports_parse(XG10_MCA).unwrap();
    let port1 = ports.iter().find(|p| p.idx == 1).unwrap();
    assert!(port1.up);
    assert_eq!(port1.poe_power_w, Some(8.45));
}

#[test]
fn mca_null_poe() {
    let ports = mca_ports_parse(XG10_MCA).unwrap();
    let port11 = ports.iter().find(|p| p.idx == 11).unwrap();
    assert_eq!(port11.poe_power_w, None);
}

#[test]
fn mca_missing_mac_table() {
    let json = r#"{"version":"1.0","model":"X","port_table":[
        {"port_idx":5,"up":false,"speed":0,"stp_state":"disabled"}
    ]}"#;
    let ports = mca_ports_parse(json).unwrap();
    assert_eq!(ports.len(), 1);
    assert!(ports[0].macs.is_empty());
}

#[test]
fn probe_parses_all_sections() {
    let (health, _ports) = probe_parse(XG10_PROBE).unwrap();
    assert!(health.version.starts_with("US3.7.5"));
    assert!(health.uptime_s > 0.0);
    assert_eq!(health.cfg_md5.len(), 32);
}

#[test]
fn probe_missing_section_errors() {
    assert!(probe_parse(MISSING_CFGMD5).is_err());
}

#[test]
fn probe_script_reads_lock_path() {
    let script = probe_script_build(Some("/var/run/switch.lock"));
    assert!(
        script.contains(
            "echo \"@@lock\"\ncat /var/run/switch.lock 2>/dev/null; echo\necho \"@@mca\"\n"
        ),
        "{script}"
    );
}

#[test]
fn probe_script_without_lock_path() {
    let script = probe_script_build(None);
    assert!(
        script.contains("echo \"@@lock\"\necho \"@@mca\"\n"),
        "{script}"
    );
    assert!(!script.contains("lock 2>/dev/null"), "{script}");
}
