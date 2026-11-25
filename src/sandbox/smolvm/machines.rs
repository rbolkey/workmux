//! Naming, labelling and finding workmux's smol machines.
//!
//! Every machine workmux creates is labelled, so it can find its own later
//! from `machine ls --json` alone (no git): which project and worktree a
//! machine serves, which agent runs in it, and which supervisor owns it.

use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

pub const LABEL_WORKMUX: &str = "workmux";
pub const LABEL_PROJECT: &str = "wm-project";
pub const LABEL_WORKTREE: &str = "wm-worktree";
pub const LABEL_AGENT: &str = "wm-agent";
pub const LABEL_OWNER: &str = "wm-owner";
/// The pack a machine was created from, so a superseded pack is kept while
/// a machine from it runs.
pub const LABEL_PACK: &str = "wm-pack";

/// The agent label of a pack's base machine.
pub const PACK_AGENT: &str = "pack";

/// smolvm's limit on machine names.
const MAX_NAME_LEN: usize = 128;
/// Room left for the project's name in a machine name, so the whole name
/// stays readable in `smolvm machine ls`.
const MAX_PROJECT_LEN: usize = 40;
const MAX_AGENT_LEN: usize = 24;

/// The process that owns a machine: a supervisor (or `sandbox shell`). The
/// start time tells a live owner from a later process that reused its PID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub pid: u32,
    pub start: u64,
}

impl Owner {
    pub fn current() -> Result<Self> {
        let pid = std::process::id();
        let start = process_start_time(pid).context("Failed to read this process's start time")?;
        Ok(Self { pid, start })
    }

    pub fn parse(label: &str) -> Option<Self> {
        let (pid, start) = label.split_once(':')?;
        Some(Self {
            pid: pid.parse().ok()?,
            start: start.parse().ok()?,
        })
    }

    /// Whether the owning process still runs: a process with its PID exists
    /// and started when the owner did.
    pub fn is_alive(&self) -> bool {
        process_start_time(self.pid) == Some(self.start)
    }
}

impl fmt::Display for Owner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.pid, self.start)
    }
}

/// A process's start time, in a unit stable for the life of the process
/// (microseconds since the epoch on macOS, clock ticks since boot on Linux).
#[cfg(target_os = "macos")]
pub fn process_start_time(pid: u32) -> Option<u64> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is a writable buffer of `size` bytes.
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (written == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

#[cfg(target_os = "linux")]
pub fn process_start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may hold spaces and parentheses; the fields after it
    // start at field 3 (state), so the start time (field 22) is the 20th.
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn process_start_time(_pid: u32) -> Option<u64> {
    None
}

/// Who a machine is for: the project, the worktree and the agent. Derived
/// from paths only, so removal can find a worktree's machines without git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineIdentity {
    project: String,
    project_hash: String,
    worktree_hash: String,
    agent: String,
}

impl MachineIdentity {
    pub fn new(project_root: &Path, worktree: &Path, agent: &str) -> Self {
        let project = project_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self {
            project: slug_within(&project, MAX_PROJECT_LEN),
            project_hash: path_hash(project_root),
            worktree_hash: path_hash(worktree),
            agent: slug_within(agent, MAX_AGENT_LEN),
        }
    }

    /// `wm-<project>-<hash8>-<agent>-<suffix>`, within smolvm's 128
    /// characters of `[A-Za-z0-9_-]`.
    pub fn name(&self, suffix: &str) -> String {
        let parts = [
            "wm",
            &self.project,
            &self.project_hash[..8],
            &self.agent,
            suffix,
        ];
        let name = parts
            .iter()
            .filter(|part| !part.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join("-");
        debug_assert!(name.len() <= MAX_NAME_LEN);
        name
    }

    /// `<project>-<hash8>`: the project, as a file name.
    pub fn project_key(&self) -> String {
        format!("{}-{}", self.project, &self.project_hash[..8])
            .trim_start_matches('-')
            .to_string()
    }

    /// `wm-pack-<project>-<hash8>`, the project's pack names before their
    /// key.
    pub fn pack_prefix(&self) -> String {
        format!("wm-pack-{}-{}", self.project, &self.project_hash[..8]).replace("--", "-")
    }

    /// A pack's base machine: workmux's, the project's and the owner's, but
    /// no worktree, so removing a worktree leaves a build alone and a base
    /// left by a killed build is swept as an orphan.
    pub fn pack_base_labels(&self, owner: &Owner) -> Vec<(String, String)> {
        [
            (LABEL_WORKMUX, "1".to_string()),
            (LABEL_PROJECT, self.project_hash.clone()),
            (LABEL_AGENT, PACK_AGENT.to_string()),
            (LABEL_OWNER, owner.to_string()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
    }

    pub fn labels(&self, owner: &Owner) -> Vec<(String, String)> {
        [
            (LABEL_WORKMUX, "1".to_string()),
            (LABEL_PROJECT, self.project_hash.clone()),
            (LABEL_WORKTREE, self.worktree_hash.clone()),
            (LABEL_AGENT, self.agent.clone()),
            (LABEL_OWNER, owner.to_string()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
    }
}

/// The value of a machine's `wm-worktree` (or `wm-project`) label: a hash
/// of the canonical path.
pub fn path_hash(path: &Path) -> String {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    hex(&Sha256::digest(canonical.to_string_lossy().as_bytes())[..8])
}

/// Lowercase hex of `bytes`.
pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Six random lowercase letters and digits, so panes of one agent in one
/// worktree get machines of their own.
pub fn random_suffix() -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 6];
    getrandom::fill(&mut bytes).expect("failed to get random bytes");
    bytes
        .iter()
        .map(|byte| ALPHABET[*byte as usize % ALPHABET.len()] as char)
        .collect()
}

fn slug_within(text: &str, max: usize) -> String {
    let slug = slug::slugify(text);
    if slug.len() <= max {
        slug
    } else {
        slug[..max].trim_end_matches('-').to_string()
    }
}

/// One machine from `smolvm machine ls --json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ListedMachine {
    pub name: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

impl ListedMachine {
    pub fn is_workmux(&self) -> bool {
        self.labels.get(LABEL_WORKMUX).is_some_and(|v| v == "1")
    }

    pub fn owner(&self) -> Option<Owner> {
        self.labels.get(LABEL_OWNER).and_then(|v| Owner::parse(v))
    }

    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.get(key).map(String::as_str)
    }

    /// Whether this machine serves the worktree whose label value is `hash`.
    pub fn serves_worktree(&self, hash: &str) -> bool {
        self.is_workmux() && self.label(LABEL_WORKTREE) == Some(hash)
    }
}

pub fn parse_machine_list(json: &[u8]) -> Result<Vec<ListedMachine>> {
    serde_json::from_slice(json).context("Failed to parse smolvm machine ls --json")
}

/// workmux's machines whose owner is gone: its process exited, its PID now
/// belongs to a later process, or the machine names no owner at all.
pub fn orphans(
    machines: &[ListedMachine],
    is_alive: impl Fn(&Owner) -> bool,
) -> Vec<&ListedMachine> {
    machines
        .iter()
        .filter(|machine| machine.is_workmux())
        .filter(|machine| !machine.owner().is_some_and(|owner| is_alive(&owner)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(name: &str, state: &str, labels: &[(&str, &str)]) -> ListedMachine {
        ListedMachine {
            name: name.into(),
            state: state.into(),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn test_machines_are_named_after_project_hash_agent_and_suffix() {
        let identity = MachineIdentity::new(
            Path::new("/src/My Project.rs"),
            Path::new("/src/My Project.rs__worktrees/feat"),
            "claude",
        );
        let name = identity.name("ab12cd");
        let hash = path_hash(Path::new("/src/My Project.rs"));
        assert_eq!(
            name,
            format!("wm-my-project-rs-{}-claude-ab12cd", &hash[..8])
        );
    }

    #[test]
    fn test_long_names_stay_within_smolvms_limit_and_alphabet() {
        let long = "x".repeat(300);
        let identity = MachineIdentity::new(
            &Path::new("/src").join(&long),
            Path::new("/src/wt"),
            &format!("agent.{long}"),
        );
        let name = identity.name(&random_suffix());
        assert!(name.len() <= 128, "{} chars", name.len());
        assert!(
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "{name}"
        );
    }

    #[test]
    fn test_random_suffixes_are_six_characters_and_differ() {
        let (a, b) = (random_suffix(), random_suffix());
        assert_eq!(a.len(), 6);
        assert_ne!(a, b);
    }

    #[test]
    fn test_machines_are_labelled_with_project_worktree_agent_and_owner() {
        let identity = MachineIdentity::new(
            Path::new("/src/proj"),
            Path::new("/src/proj__worktrees/feat"),
            "codex",
        );
        let labels = identity.labels(&Owner {
            pid: 42,
            start: 1700,
        });
        let labels: BTreeMap<_, _> = labels.into_iter().collect();
        assert_eq!(labels["workmux"], "1");
        assert_eq!(labels["wm-project"], path_hash(Path::new("/src/proj")));
        assert_eq!(
            labels["wm-worktree"],
            path_hash(Path::new("/src/proj__worktrees/feat"))
        );
        assert_ne!(labels["wm-project"], labels["wm-worktree"]);
        assert_eq!(labels["wm-agent"], "codex");
        assert_eq!(labels["wm-owner"], "42:1700");
    }

    #[test]
    fn test_this_process_is_a_live_owner_and_a_reused_pid_is_not() {
        let me = Owner::current().unwrap();
        assert!(me.is_alive());
        assert!(
            !Owner {
                start: me.start + 1,
                ..me
            }
            .is_alive()
        );
        assert_eq!(Owner::parse(&me.to_string()), Some(me));
    }

    #[test]
    fn test_parses_machine_ls_json() {
        let json = br#"[
            {"name": "plain", "state": "created", "labels": {}, "cpus": 4},
            {"name": "wm-a", "state": "running",
             "labels": {"workmux": "1", "wm-owner": "12:34", "wm-worktree": "ab"}}
        ]"#;
        let machines = parse_machine_list(json).unwrap();
        assert_eq!(machines.len(), 2);
        assert!(!machines[0].is_workmux());
        assert!(machines[1].is_workmux());
        assert_eq!(machines[1].owner(), Some(Owner { pid: 12, start: 34 }));
        assert!(machines[1].serves_worktree("ab"));
        assert!(!machines[0].serves_worktree("ab"));
    }

    #[test]
    fn test_orphans_are_workmux_machines_without_a_live_owner() {
        let machines = [
            machine("live", "running", &[("workmux", "1"), ("wm-owner", "1:10")]),
            machine("dead", "running", &[("workmux", "1"), ("wm-owner", "2:20")]),
            machine(
                "reused",
                "stopped",
                &[("workmux", "1"), ("wm-owner", "1:11")],
            ),
            machine("unowned", "created", &[("workmux", "1")]),
            machine("theirs", "running", &[("wm-owner", "2:20")]),
        ];
        let live = Owner { pid: 1, start: 10 };
        let names: Vec<_> = orphans(&machines, |owner| *owner == live)
            .into_iter()
            .map(|m| m.name.as_str())
            .collect();
        assert_eq!(names, ["dead", "reused", "unowned"]);
    }
}
