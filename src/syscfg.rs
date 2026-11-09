// Parse, change and render a switch system.cfg file without losing any line.

use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq)]
enum Line {
    Kv { key: String, value: String },
    Other(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SysCfg {
    lines: Vec<Line>,
    trailing_newline: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct KeyChange {
    pub key: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

fn body_lines_split(text: &str) -> (Vec<&str>, bool) {
    if text.is_empty() {
        return (Vec::new(), false);
    }
    let trailing_newline = text.ends_with('\n');
    let body = if trailing_newline {
        &text[..text.len() - 1]
    } else {
        text
    };
    (body.split('\n').collect(), trailing_newline)
}

// A key's group is everything up to its last `.`; a flat key (no `.`) groups with other
// flat keys under the empty prefix.
fn key_group_prefix(key: &str) -> &str {
    match key.rfind('.') {
        Some(idx) => &key[..idx],
        None => "",
    }
}

pub fn syscfg_parse(text: &str) -> SysCfg {
    let (raw_lines, trailing_newline) = body_lines_split(text);
    let lines = raw_lines
        .into_iter()
        .map(|line| match (line.starts_with('#'), line.find('=')) {
            (false, Some(eq_idx)) => Line::Kv {
                key: line[..eq_idx].to_string(),
                value: line[eq_idx + 1..].to_string(),
            },
            _ => Line::Other(line.to_string()),
        })
        .collect();
    SysCfg {
        lines,
        trailing_newline,
    }
}

pub fn syscfg_render(cfg: &SysCfg) -> String {
    let mut out = String::new();
    for (i, line) in cfg.lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        match line {
            Line::Kv { key, value } => {
                out.push_str(key);
                out.push('=');
                out.push_str(value);
            }
            Line::Other(text) => out.push_str(text),
        }
    }
    if cfg.trailing_newline {
        out.push('\n');
    }
    out
}

pub fn syscfg_get<'a>(cfg: &'a SysCfg, key: &str) -> Option<&'a str> {
    cfg.lines.iter().rev().find_map(|line| match line {
        Line::Kv { key: k, value } if k == key => Some(value.as_str()),
        _ => None,
    })
}

pub fn syscfg_keys_list(cfg: &SysCfg) -> Vec<&str> {
    let mut seen = HashSet::new();
    cfg.lines
        .iter()
        .filter_map(|line| match line {
            Line::Kv { key, .. } => Some(key.as_str()),
            Line::Other(_) => None,
        })
        .filter(|key| seen.insert(*key))
        .collect()
}

pub fn syscfg_set(cfg: &mut SysCfg, key: &str, value: &str) {
    let mut found = false;
    for line in cfg.lines.iter_mut() {
        if let Line::Kv { key: k, value: v } = line
            && k == key
        {
            *v = value.to_string();
            found = true;
        }
    }
    if found {
        return;
    }

    let prefix = key_group_prefix(key);
    let insert_after = cfg.lines.iter().enumerate().rev().find_map(|(i, line)| {
        if let Line::Kv { key: k, .. } = line
            && key_group_prefix(k) == prefix
        {
            return Some(i);
        }
        None
    });
    let new_line = Line::Kv {
        key: key.to_string(),
        value: value.to_string(),
    };
    match insert_after {
        Some(i) => cfg.lines.insert(i + 1, new_line),
        None => cfg.lines.push(new_line),
    }
}

// Renames in place, so that the line keeps its position in the file.
pub fn syscfg_rename(cfg: &mut SysCfg, old: &str, new: &str) {
    for line in cfg.lines.iter_mut() {
        if let Line::Kv { key, .. } = line
            && key == old
        {
            *key = new.to_string();
        }
    }
}

pub fn syscfg_remove(cfg: &mut SysCfg, key: &str) {
    cfg.lines.retain(|line| match line {
        Line::Kv { key: k, .. } => k != key,
        Line::Other(_) => true,
    });
}

pub fn syscfg_diff(old: &SysCfg, new: &SysCfg) -> Vec<KeyChange> {
    let mut result = Vec::new();
    let mut seen_in_new = HashSet::new();

    for line in &new.lines {
        let Line::Kv { key, .. } = line else {
            continue;
        };
        if !seen_in_new.insert(key.clone()) {
            continue;
        }
        let new_value = syscfg_get(new, key).map(str::to_string);
        let old_value = syscfg_get(old, key).map(str::to_string);
        if old_value != new_value {
            result.push(KeyChange {
                key: key.clone(),
                old: old_value,
                new: new_value,
            });
        }
    }

    let mut seen_in_old = HashSet::new();
    for line in &old.lines {
        let Line::Kv { key, .. } = line else {
            continue;
        };
        if seen_in_new.contains(key) || !seen_in_old.insert(key.clone()) {
            continue;
        }
        result.push(KeyChange {
            key: key.clone(),
            old: syscfg_get(old, key).map(str::to_string),
            new: None,
        });
    }

    result
}
