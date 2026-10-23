//! Agent processes in remote panes, found and reaped by remote workmux: the
//! agents run on the remote, so their PIDs mean nothing on the workstation.
//!
//! An agent is the topmost process in a remote pane whose command is a known
//! agent or the project's configured one. Its last activity is the last time
//! its terminal was read from or written to, as `w` reports idle time.

use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, UNIX_EPOCH};

use super::pane::{RemotePaneId, SessionHolder};

/// Commands that run an agent's script, so the script names the agent.
const INTERPRETERS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "node", "python", "python3", "bun", "deno", "perl",
];

/// An agent process running in a remote pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAgent {
    pub pane: RemotePaneId,
    pub pid: i32,
    /// The agent's name, from its command.
    pub name: String,
    tty: Option<PathBuf>,
}

impl RemoteAgent {
    /// When the agent's terminal was last read from or written to, in
    /// seconds since the epoch.
    pub fn last_activity(&self) -> Option<u64> {
        let meta = std::fs::metadata(self.tty.as_ref()?).ok()?;
        [meta.accessed(), meta.modified()]
            .into_iter()
            .filter_map(|time| time.ok()?.duration_since(UNIX_EPOCH).ok())
            .map(|since| since.as_secs())
            .max()
    }

    /// Ask the agent to exit, and kill it if it has not within two seconds.
    /// Its remote pane stays, as a local pane does when its agent exits.
    pub fn exit(&self) -> Result<()> {
        unsafe { libc::kill(self.pid, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(2);
        while alive(self.pid) {
            if Instant::now() >= deadline {
                unsafe { libc::kill(self.pid, libc::SIGKILL) };
                std::thread::sleep(Duration::from_millis(100));
                if alive(self.pid) {
                    return Err(anyhow!("agent {} did not exit", self.pid));
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }
}

fn alive(pid: i32) -> bool {
    // A zombie has exited; only its parent's wait is missing.
    let signalled = unsafe { libc::kill(pid, 0) } == 0;
    signalled
        && !Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).trim_start().starts_with('Z'))
}

/// The agents running in `project`'s remote panes, one per pane at most.
/// `configured` is the project's agent command, recognised besides the
/// known agents.
pub fn agents(
    holder: &SessionHolder,
    project: &str,
    configured: Option<&str>,
) -> Result<Vec<RemoteAgent>> {
    let panes = holder.running(project)?;
    if panes.is_empty() {
        return Ok(Vec::new());
    }
    let table = process_table()?;
    let configured = configured.and_then(command_name);
    Ok(panes
        .into_iter()
        .filter_map(|pane| {
            let root = holder.pid(&pane)?;
            let agent = table.topmost(root, |process| {
                process.names().into_iter().find(|name| {
                    configured.as_deref() == Some(name.as_str())
                        || crate::agent_identity::classify_agent_kind(Some(name), None).is_some()
                })
            })?;
            Some(RemoteAgent {
                pane,
                pid: agent.0.pid,
                name: agent.1,
                tty: agent.0.tty_path(),
            })
        })
        .collect())
}

#[derive(Debug, Clone)]
struct Process {
    pid: i32,
    ppid: i32,
    tty: String,
    args: String,
}

impl Process {
    /// Names the process's command goes by: its program, and the script an
    /// interpreter runs.
    fn names(&self) -> Vec<String> {
        let mut words = self.args.split_whitespace();
        let mut names = Vec::new();
        if let Some(program) = words.next().and_then(stem) {
            if INTERPRETERS.contains(&program.as_str()) {
                if let Some(script) = words.find(|word| !word.starts_with('-')).and_then(stem) {
                    names.push(script);
                }
            }
            names.push(program);
        }
        names
    }

    fn tty_path(&self) -> Option<PathBuf> {
        (!self.tty.is_empty() && !self.tty.starts_with('?'))
            .then(|| Path::new("/dev").join(&self.tty))
    }
}

fn stem(word: &str) -> Option<String> {
    let name = Path::new(word).file_name()?.to_str()?;
    Some(name.trim_start_matches('-').to_string()).filter(|n| !n.is_empty())
}

/// The name of the program a configured agent command runs.
fn command_name(command: &str) -> Option<String> {
    stem(&crate::multiplexer::agent::find_executable_token(command))
}

struct ProcessTable {
    children: HashMap<i32, Vec<Process>>,
    by_pid: HashMap<i32, Process>,
}

impl ProcessTable {
    /// The first process, breadth first from `root`, that `name` names.
    fn topmost(
        &self,
        root: i32,
        name: impl Fn(&Process) -> Option<String>,
    ) -> Option<(Process, String)> {
        let mut queue = std::collections::VecDeque::from([self.by_pid.get(&root)?.clone()]);
        while let Some(process) = queue.pop_front() {
            if let Some(found) = name(&process) {
                return Some((process, found));
            }
            queue.extend(self.children.get(&process.pid).into_iter().flatten().cloned());
        }
        None
    }
}

fn process_table() -> Result<ProcessTable> {
    let output = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,tty=,args="])
        .output()
        .context("failed to list processes")?;
    Ok(parse_process_table(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_process_table(text: &str) -> ProcessTable {
    let mut table = ProcessTable {
        children: HashMap::new(),
        by_pid: HashMap::new(),
    };
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(tty)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse(), ppid.parse()) else {
            continue;
        };
        let process = Process {
            pid,
            ppid,
            tty: tty.to_string(),
            args: fields.collect::<Vec<_>>().join(" "),
        };
        table
            .children
            .entry(process.ppid)
            .or_default()
            .push(process.clone());
        table.by_pid.insert(pid, process);
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::pane::PaneState;

    #[test]
    fn an_agent_is_found_in_its_remote_pane_and_exits_leaving_the_pane() {
        let Some((temp, holder)) = crate::remote::pane::tests::holder() else {
            return;
        };
        let worktree = temp.path().join("feature");
        std::fs::create_dir_all(worktree.join(".git")).unwrap();
        let agent = temp.path().join("fake-agent");
        std::fs::write(&agent, "while :; do sleep 1; done\n").unwrap();
        let argv = |script: &str| ["sh", "-c", script].map(String::from).to_vec();
        let with_agent = holder
            .create(
                "api",
                &worktree,
                &argv(&format!("sh {} --flag; exec sleep 30", agent.display())),
            )
            .unwrap();
        let shell = holder
            .create("api", &worktree, &argv("exec sleep 30"))
            .unwrap();
        std::thread::sleep(Duration::from_millis(300));

        let found = agents(&holder, "api", Some("env X=1 fake-agent --yolo")).unwrap();

        let found_panes: Vec<_> = found.iter().map(|a| (&a.pane, a.name.as_str())).collect();
        assert_eq!(found_panes, vec![(&with_agent, "fake-agent")]);
        let fake = &found[0];
        assert!(fake.last_activity().is_some());
        fake.exit().unwrap();
        assert!(!alive(fake.pid));
        assert_eq!(holder.state(&with_agent).unwrap(), PaneState::Running);
        for id in [with_agent, shell] {
            holder.kill(&id).unwrap();
        }
    }

    #[test]
    fn an_interpreted_agent_is_named_by_its_script_and_known_agents_need_no_config() {
        let table = parse_process_table(
            " 10 1 ttys001 -zsh\n\
             11 10 ttys001 node /opt/bin/codex --yolo\n\
             12 11 ttys001 rg foo\n\
             20 1 ?? /usr/bin/claude\n",
        );
        let known = |process: &Process| {
            process
                .names()
                .into_iter()
                .find(|name| crate::agent_identity::classify_agent_kind(Some(name), None).is_some())
        };

        let (codex, name) = table.topmost(10, known).unwrap();

        assert_eq!((codex.pid, name.as_str()), (11, "codex"));
        assert_eq!(
            codex.tty_path().as_deref(),
            Some(Path::new("/dev/ttys001"))
        );
        assert_eq!(table.topmost(20, known).unwrap().1, "claude");
        assert!(table.topmost(12, known).is_none());
        assert_eq!(table.by_pid[&20].tty_path(), None);
    }
}
