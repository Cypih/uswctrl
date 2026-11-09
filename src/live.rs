// Parse the switch's live port state (mca-dump) and the health probe output.

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortLive {
    pub idx: u8,
    pub up: bool,
    pub speed_mbps: u32,
    pub poe_power_w: Option<f32>,
    pub poe_class: Option<String>,
    pub stp_state: String,
    pub macs: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Health {
    pub version: String,
    pub uptime_s: f64,
    pub cfg_md5: String,
    pub mem_avail_kb: u64,
    pub offline_files: Vec<String>,
    pub inform_status: String,
    pub lock: Option<String>,
    pub firmware: String,
    pub model: String,
}

// Order matches the plain-text sections below; `@@mca` stays last because its JSON is
// large and the parser reads everything after that marker as one block.
pub const PROBE_HEAD: &str = "\
echo \"@@version\"
cat /etc/version
echo \"@@uptime\"
cat /proc/uptime
echo \"@@cfgmd5\"
md5sum /tmp/system.cfg
echo \"@@mem\"
grep -E '^(MemAvailable|MemFree):' /proc/meminfo
echo \"@@offline\"
ls /etc/persistent/offline_logs/*/ 2>/dev/null
echo \"@@inform\"
mca-cli-op info | grep Status
echo \"@@lock\"
";
const PROBE_TAIL: &str = "\
echo \"@@mca\"
mca-dump
";

/// The health probe script. Without `lock_path` the `@@lock` section stays empty, which
/// reads as no lock. `config::config_load` allows only plain path characters in the path.
pub fn probe_script_build(lock_path: Option<&str>) -> String {
    let lock_line = match lock_path {
        // The `echo` ends a lock text that has no final newline, so that the next marker
        // starts a line of its own.
        Some(path) => format!("cat {path} 2>/dev/null; echo\n"),
        None => String::new(),
    };
    format!("{PROBE_HEAD}{lock_line}{PROBE_TAIL}")
}

#[derive(Deserialize)]
struct McaMac {
    mac: String,
}

#[derive(Deserialize)]
struct McaPort {
    port_idx: u8,
    up: bool,
    speed: u32,
    #[serde(default)]
    poe_power: Option<String>,
    #[serde(default)]
    poe_class: Option<String>,
    stp_state: String,
    #[serde(default)]
    mac_table: Option<Vec<McaMac>>,
}

#[derive(Deserialize)]
struct McaDump {
    #[serde(default)]
    version: String,
    #[serde(default)]
    model: String,
    port_table: Vec<McaPort>,
}

fn mca_ports_build(ports: Vec<McaPort>) -> anyhow::Result<Vec<PortLive>> {
    ports
        .into_iter()
        .map(|port| {
            let poe_power_w = port
                .poe_power
                .as_deref()
                .map(str::parse::<f32>)
                .transpose()
                .with_context(|| format!("parse poe_power of port {}", port.port_idx))?;
            let macs = port
                .mac_table
                .unwrap_or_default()
                .into_iter()
                .map(|entry| entry.mac)
                .collect();
            Ok(PortLive {
                idx: port.port_idx,
                up: port.up,
                speed_mbps: port.speed,
                poe_power_w,
                poe_class: port.poe_class,
                stp_state: port.stp_state,
                macs,
            })
        })
        .collect()
}

pub fn mca_ports_parse(json: &str) -> anyhow::Result<Vec<PortLive>> {
    let dump: McaDump = serde_json::from_str(json).context("parse mca-dump JSON")?;
    mca_ports_build(dump.port_table)
}

// Splits the probe output on its `@@name` marker lines. The body of a section runs up to
// the next marker line, or to the end of the text for the last one (`@@mca`), so the JSON
// there keeps its original bytes.
fn probe_sections_split(text: &str) -> Vec<(&str, &str)> {
    let mut markers: Vec<(&str, usize, usize)> = Vec::new();
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let trimmed = line.strip_suffix('\n').unwrap_or(line);
        if let Some(name) = trimmed.strip_prefix("@@") {
            markers.push((name, offset, offset + line.len()));
        }
        offset += line.len();
    }
    markers
        .iter()
        .enumerate()
        .map(|(i, (name, _line_start, body_start))| {
            let end = markers
                .get(i + 1)
                .map_or(text.len(), |(_, line_start, _)| *line_start);
            (*name, &text[*body_start..end])
        })
        .collect()
}

fn probe_section_get<'a>(sections: &[(&str, &'a str)], name: &str) -> anyhow::Result<&'a str> {
    sections
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, body)| *body)
        .ok_or_else(|| anyhow::anyhow!("probe output has no @@{name} section"))
}

fn mem_avail_kb_parse(section: &str) -> anyhow::Result<u64> {
    let line = section
        .lines()
        .find(|line| line.trim_start().starts_with("MemAvailable:"))
        .or_else(|| {
            section
                .lines()
                .find(|line| line.trim_start().starts_with("MemFree:"))
        })
        .ok_or_else(|| anyhow::anyhow!("@@mem section has no MemAvailable or MemFree line"))?;
    line.split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("@@mem line {line:?} has no value"))?
        .parse()
        .with_context(|| format!("parse @@mem line {line:?}"))
}

// Each `ls` directory header ends with `:`; every other non-empty line is a file name.
fn offline_files_parse(section: &str) -> Vec<String> {
    let mut files: Vec<String> = section
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.ends_with(':'))
        .map(str::to_string)
        .collect();
    files.sort();
    files
}

fn inform_status_parse(section: &str) -> String {
    section
        .lines()
        .find_map(|line| line.trim().strip_prefix("Status:"))
        .map_or_else(
            || section.trim().to_string(),
            |rest| rest.trim().to_string(),
        )
}

fn lock_parse(section: &str) -> Option<String> {
    let trimmed = section.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub fn probe_parse(text: &str) -> anyhow::Result<(Health, Vec<PortLive>)> {
    let sections = probe_sections_split(text);

    let version = probe_section_get(&sections, "version")?.trim().to_string();
    let uptime_s = probe_section_get(&sections, "uptime")?
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("@@uptime section is empty"))?
        .parse()
        .context("parse @@uptime")?;
    let cfg_md5 = probe_section_get(&sections, "cfgmd5")?
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("@@cfgmd5 section is empty"))?
        .to_string();
    let mem_avail_kb = mem_avail_kb_parse(probe_section_get(&sections, "mem")?)?;
    let offline_files = offline_files_parse(probe_section_get(&sections, "offline")?);
    let inform_status = inform_status_parse(probe_section_get(&sections, "inform")?);
    let lock = lock_parse(probe_section_get(&sections, "lock")?);

    let mca_text = probe_section_get(&sections, "mca")?;
    let dump: McaDump = serde_json::from_str(mca_text).context("parse @@mca JSON")?;
    let firmware = dump.version.clone();
    let model = dump.model.clone();
    let ports = mca_ports_build(dump.port_table)?;

    Ok((
        Health {
            version,
            uptime_s,
            cfg_md5,
            mem_avail_kb,
            offline_files,
            inform_status,
            lock,
            firmware,
            model,
        },
        ports,
    ))
}
