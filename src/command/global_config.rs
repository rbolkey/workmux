//! Edits to the global config file's text for `workmux remote` and
//! `workmux project`, keeping the rest of the file (including comments) as
//! it is. Each edit re-parses its result and refuses to write a file that
//! doesn't match the intended change.

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::config::{Config, ProjectEntry, RemoteConfig};

/// A top-level map of named entries in global config.
pub struct Section<T: 'static> {
    /// The section's key, such as `remotes`.
    pub key: &'static str,
    /// What one entry is called in messages, such as `remote`.
    pub noun: &'static str,
    pub entries: fn(&Config) -> &BTreeMap<String, T>,
}

pub const REMOTES: Section<RemoteConfig> = Section {
    key: "remotes",
    noun: "remote",
    entries: |config| &config.remotes,
};

pub const PROJECTS: Section<ProjectEntry> = Section {
    key: "projects",
    noun: "project",
    entries: |config| &config.projects,
};

/// The global config file's path and text (empty when it doesn't exist).
pub fn read() -> Result<(PathBuf, String)> {
    let path = crate::config::global_config_path()
        .ok_or_else(|| anyhow!("could not determine your home directory"))?;
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    Ok((path, content))
}

pub fn write(path: &PathBuf, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))
}

pub fn parse(content: &str) -> Result<Config> {
    if content.trim().is_empty() {
        return Ok(Config::default());
    }
    serde_yaml::from_str(content).context("global config does not parse")
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// The top-level `<key>:` block: the header line's index, the index just
/// past its last line, and the indent of its entries.
fn find_section(lines: &[&str], key: &str) -> Result<Option<(usize, usize, Option<usize>)>> {
    let header_text = format!("{key}:");
    let Some(header) = lines.iter().position(|line| line.starts_with(&header_text)) else {
        return Ok(None);
    };
    let rest = lines[header][header_text.len()..].trim();
    if !(rest.is_empty() || rest.starts_with('#')) {
        bail!("the {key} section of your global config is not a block mapping; edit it by hand");
    }
    let end = lines[header + 1..]
        .iter()
        .position(|line| !is_blank(line) && indent_of(line) == 0)
        .map_or(lines.len(), |offset| header + 1 + offset);
    let indent = lines[header + 1..end]
        .iter()
        .find(|line| !is_blank(line) && !line.trim_start().starts_with('#'))
        .map(|line| indent_of(line));
    Ok(Some((header, end, indent)))
}

fn join_lines(lines: &[String], trailing_newline: bool) -> String {
    let mut joined = lines.join("\n");
    if trailing_newline && !joined.is_empty() {
        joined.push('\n');
    }
    joined
}

/// Add entry `name` to `section` of a global config file's text.
pub fn add_entry<T: Serialize + PartialEq>(
    content: &str,
    section: &Section<T>,
    name: &str,
    value: &T,
) -> Result<String> {
    let before = parse(content)?;
    if (section.entries)(&before).contains_key(name) {
        bail!("{} {name} already exists", section.noun);
    }
    let entry = serde_yaml::to_string(&BTreeMap::from([(name, value)]))?;

    let lines: Vec<&str> = content.lines().collect();
    let mut updated: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
    match find_section(&lines, section.key)? {
        Some((header, end, indent)) => {
            let pad = " ".repeat(indent.unwrap_or(2));
            // Directly after the section's last non-blank line.
            let at = (header + 1..end)
                .rev()
                .find(|&i| !is_blank(lines[i]))
                .map_or(header + 1, |i| i + 1);
            let entry_lines = entry.lines().map(|line| format!("{pad}{line}"));
            updated.splice(at..at, entry_lines);
        }
        None => {
            updated.push(format!("{}:", section.key));
            updated.extend(entry.lines().map(|line| format!("  {line}")));
        }
    }
    let updated = join_lines(&updated, true);

    let added = parse(&updated).ok().is_some_and(|after| {
        let (before, after) = ((section.entries)(&before), (section.entries)(&after));
        after.get(name) == Some(value) && after.len() == before.len() + 1
    });
    if !added {
        bail!(
            "could not add {} {name} to your global config automatically; add it by hand",
            section.noun
        );
    }
    Ok(updated)
}

/// Remove entry `name` from `section` of a global config file's text.
pub fn remove_entry<T>(content: &str, section: &Section<T>, name: &str) -> Result<String> {
    let noun = section.noun;
    let before = parse(content)?;
    if !(section.entries)(&before).contains_key(name) {
        bail!("no {noun} named {name}");
    }
    let lines: Vec<&str> = content.lines().collect();
    let (header, end, indent) = find_section(&lines, section.key)?
        .ok_or_else(|| anyhow!("no {} section in your global config", section.key))?;
    let indent = indent.unwrap_or(2);
    let is_entry_line = |line: &str| {
        indent_of(line) == indent && {
            let key = line.trim_start();
            [
                format!("{name}:"),
                format!("\"{name}\":"),
                format!("'{name}':"),
            ]
            .iter()
            .any(|prefix| {
                key.strip_prefix(prefix.as_str())
                    .is_some_and(|rest| rest.trim().is_empty() || rest.trim().starts_with('#'))
            })
        }
    };
    let start = (header + 1..end)
        .find(|&i| is_entry_line(lines[i]))
        .ok_or_else(|| {
            anyhow!("could not find {noun} {name} in your global config; remove it by hand")
        })?;
    let stop = (start + 1..end)
        .find(|&i| !is_blank(lines[i]) && indent_of(lines[i]) <= indent)
        .unwrap_or(end);

    let mut kept: Vec<&str> = lines.clone();
    kept.drain(start..stop);
    let section_end = end - (stop - start);
    let section_empty = kept[header + 1..section_end]
        .iter()
        .all(|line| is_blank(line) || line.trim_start().starts_with('#'));
    if section_empty {
        kept.drain(header..section_end);
    }
    let updated = join_lines(
        &kept.iter().map(|line| line.to_string()).collect::<Vec<_>>(),
        content.ends_with('\n'),
    );

    let removed = parse(&updated).ok().is_some_and(|after| {
        let (before, after) = ((section.entries)(&before), (section.entries)(&after));
        !after.contains_key(name) && after.len() + 1 == before.len()
    });
    if !removed {
        bail!(
            "could not remove {noun} {name} from your global config automatically; remove it by hand"
        );
    }
    Ok(updated)
}
