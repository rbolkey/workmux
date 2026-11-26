//! Runs a [`LaunchPlan`] against the `smolvm` on `PATH`.

use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tracing::{debug, info, warn};

use super::machines::{ListedMachine, parse_machine_list};
use super::pack::{
    PackBuild, PackContext, PackWhen, mark_current, remove_pack_context, write_pack_context,
};
use super::plan::{Invocation, LaunchPlan, StepKind, delete_machine};

/// Internal override, for workmux's tests and development only: keeps
/// smolvm's state under this directory instead of the user's default one.
/// Not a config key, and not documented for users.
pub const STATE_DIR_ENV: &str = "WORKMUX_SMOLVM_STATE_DIR";

pub struct Runner {
    state_dir: Option<PathBuf>,
}

impl Runner {
    pub fn from_env() -> Self {
        Self {
            state_dir: std::env::var_os(STATE_DIR_ENV)
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from),
        }
    }

    fn command(&self, invocation: &Invocation) -> Command {
        let mut command = Command::new("smolvm");
        // smolvm's own failures as `{"error":{"code","message"}}` on stderr,
        // which [`smolvm_error`] reads.
        command.args(["--error-format", "json"]);
        if let Some(dir) = &self.state_dir {
            command.arg(OsString::from("--state-dir")).arg(dir);
        }
        command.args(&invocation.args).envs(
            invocation
                .env
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        );
        command
    }

    /// The machines smolvm knows, from `machine ls --json`.
    pub fn list(&self) -> Result<Vec<ListedMachine>> {
        let output = self
            .command(&Invocation::new(
                ["machine", "ls", "--json"].map(String::from).to_vec(),
            ))
            .output()
            .context("Failed to run smolvm machine ls")?;
        if !output.status.success() {
            bail!(
                "smolvm machine ls failed ({}): {}",
                output.status,
                smolvm_error(&output.stderr)
            );
        }
        parse_machine_list(&output.stdout)
    }

    /// Delete a machine, returning smolvm's error if it can't.
    pub fn delete(&self, machine: &str) -> Result<()> {
        let output = self
            .command(&delete_machine(machine))
            .output()
            .context("Failed to run smolvm machine delete")?;
        if !output.status.success() {
            bail!("{}", smolvm_error(&output.stderr));
        }
        Ok(())
    }

    /// Run an invocation in the foreground with the terminal attached, and
    /// return its exit code.
    pub fn run_foreground(&self, invocation: &Invocation) -> Result<i32> {
        let status = self
            .command(invocation)
            .status()
            .context("Failed to run smolvm")?;
        Ok(status.code().unwrap_or(1))
    }

    /// Run `plan` in phases: delete the orphans, build the project pack if
    /// it's missing, run the launch steps, then the exec in the foreground,
    /// then the delete, whatever the exec's exit status. Returns the exec's
    /// exit code. A failed step fails the launch: the agent never runs
    /// outside the machine.
    ///
    /// Closing the pane hangs up on the supervisor. From the first step on,
    /// the hangup is caught and passed on to the smolvm call running then
    /// (the exec client ends the guest's process), so the supervisor lives
    /// on to delete what it created: the pane's machine, or a pack build's
    /// base.
    pub fn run(&self, plan: &LaunchPlan) -> Result<i32> {
        for note in &plan.notes {
            info!(machine = %plan.machine, "{note}");
        }
        // Declared first, so it outlives the guard's delete.
        let signals = CaughtSignals::install()?;
        let (orphans, steps): (Vec<_>, Vec<_>) = plan
            .prepare
            .iter()
            .partition(|step| step.kind == StepKind::DeleteOrphan);
        for step in orphans {
            self.delete_orphan(&step.invocation, &signals)?;
        }
        // After the orphans, so a base left by a killed build is gone first.
        if plan.pack.when == PackWhen::BuildFirst {
            self.build_pack_catching(&plan.pack.build, &signals)?;
        }
        let mut owned = None;
        for step in steps {
            if step.kind == StepKind::CreateMachine {
                owned = Some(MachineGuard {
                    runner: self,
                    machine: &plan.machine,
                    delete: &plan.delete,
                });
            }
            debug!(machine = %plan.machine, args = ?step.invocation.args, "smolvm");
            let output = self.output(&step.invocation, &signals);
            signals.check()?;
            step_result(&step.invocation, &output?)?;
        }

        debug!(machine = %plan.machine, args = ?plan.exec.args, "smolvm exec");
        let mut child = self
            .command(&plan.exec)
            .spawn()
            .context("Failed to run smolvm machine exec")?;
        let status = wait_passing_on_hangup(&mut child, &signals)?;
        let exit_code = status.code().unwrap_or(1);
        info!(machine = %plan.machine, exit_code, "agent command exited");
        drop(owned);
        Ok(exit_code)
    }

    /// Delete a machine whose owner is gone. Best effort: a failure is
    /// logged and the launch goes on; only a hangup stops it.
    fn delete_orphan(&self, invocation: &Invocation, signals: &CaughtSignals) -> Result<()> {
        debug!(args = ?invocation.args, "smolvm");
        let output = self.output(invocation, signals);
        signals.check()?;
        match output {
            Ok(output) if output.status.success() => {
                info!(args = ?invocation.args, "deleted orphaned machine");
            }
            Ok(output) => warn!(
                args = ?invocation.args,
                error = %smolvm_error(&output.stderr),
                "failed to delete orphaned machine"
            ),
            Err(error) => warn!(%error, "failed to delete orphaned machine"),
        }
        Ok(())
    }

    /// Run an invocation to completion, capturing its output (as
    /// `Command::output`), passing a caught hangup on to it.
    fn output(
        &self,
        invocation: &Invocation,
        signals: &CaughtSignals,
    ) -> Result<std::process::Output> {
        use std::io::Read;
        use std::process::Stdio;
        let mut child = self
            .command(invocation)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to run smolvm")?;
        fn read_all(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                if let Some(mut pipe) = pipe {
                    let _ = pipe.read_to_end(&mut bytes);
                }
                bytes
            })
        }
        let stdout = read_all(child.stdout.take());
        let stderr = read_all(child.stderr.take());
        let status = wait_passing_on_hangup(&mut child, signals)?;
        Ok(std::process::Output {
            status,
            stdout: stdout.join().unwrap_or_default(),
            stderr: stderr.join().unwrap_or_default(),
        })
    }
}

impl Runner {
    /// Build a project pack under its lock, with progress on the terminal
    /// (stderr). A pack that appeared while waiting for the lock is used
    /// as is. The base machine is deleted
    /// whatever happens, an interrupt or hangup included; the pack is moved
    /// into place only once complete.
    pub fn build_pack(&self, build: &PackBuild) -> Result<Option<String>> {
        let signals = CaughtSignals::install()?;
        self.build_pack_catching(build, &signals)
    }

    fn build_pack_catching(
        &self,
        build: &PackBuild,
        signals: &CaughtSignals,
    ) -> Result<Option<String>> {
        use nix::fcntl::{Flock, FlockArg};
        std::fs::create_dir_all(&build.dir)
            .with_context(|| format!("Failed to create {}", build.dir.display()))?;
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&build.lock)
            .with_context(|| format!("Failed to open {}", build.lock.display()))?;
        let _lock = Flock::lock(lock_file, FlockArg::LockExclusive)
            .map_err(|(_, errno)| errno)
            .with_context(|| format!("Failed to lock {}", build.lock.display()))?;
        signals.check()?;
        if build.sidecar.exists() {
            info!(pack = %build.name, "pack built while waiting for its lock");
            return Ok(None);
        }

        let started = std::time::Instant::now();
        eprintln!(
            "workmux: building the smolvm pack {} from {}",
            build.name, build.image
        );
        // A base and output left by a killed build have these names; the
        // lock says no one else is using them.
        let _ = self.command(&build.delete_base).output();
        let _ = std::fs::remove_file(&build.output);
        let _ = std::fs::remove_file(build.output_sidecar());
        // Dropped after the base is deleted, so its mount goes first. The
        // pack's shell is warmed best effort: whatever stops it leaves a
        // cold pack and a warning, and panes build their own shells.
        let _context = build.context.as_ref().map(ContextGuard);
        let mut cold_reason = None;
        if let Some(context) = &build.context
            && let Err(error) = write_pack_context(context)
        {
            cold_reason = Some(format!("{error:#}"));
        }
        let _base = MachineGuard {
            runner: self,
            machine: &build.name,
            delete: &build.delete_base,
        };
        for step in &build.steps {
            if step.warm_up && cold_reason.is_some() {
                continue;
            }
            eprintln!("workmux:   {}", step.label);
            debug!(pack = %build.name, args = ?step.invocation.args, "smolvm");
            let (status, stderr) = self.run_streaming(&step.invocation, signals)?;
            signals.check()?;
            if status.success() {
                continue;
            }
            if step.warm_up {
                cold_reason = Some(format!(
                    "failed to {} ({status}): {}",
                    step.label,
                    tool_error(&stderr)
                ));
                continue;
            }
            bail!(
                "Building the smolvm pack {} failed to {} ({status}): {}",
                build.name,
                step.label,
                smolvm_error(stderr.as_bytes())
            );
        }
        std::fs::rename(build.output_sidecar(), &build.sidecar).with_context(|| {
            format!(
                "Failed to move {} into place",
                build.output_sidecar().display()
            )
        })?;
        // smolvm's stub executable: machines are created from the sidecar.
        let _ = std::fs::remove_file(&build.output);
        mark_current(build)?;
        eprintln!(
            "workmux: built the smolvm pack {} in {:.1}s",
            build.name,
            started.elapsed().as_secs_f64()
        );
        let warning = cold_reason.map(|why| {
            format!(
                "the smolvm pack {}'s shell isn't warm, so each pane builds its own: {why}\n\
                 The pack stays cold until its key changes; `workmux sandbox build` or \
                 `workmux sandbox pull` marks the image's packs stale to rebuild it.",
                build.name
            )
        });
        if let Some(warning) = &warning {
            warn!(pack = %build.name, "{warning}");
            eprintln!("workmux: warning: {warning}");
        }
        Ok(warning)
    }

    /// Run an invocation, echoing its output to stderr as it comes, and
    /// return its status and stderr. A caught hangup is passed on to it.
    fn run_streaming(
        &self,
        invocation: &Invocation,
        signals: &CaughtSignals,
    ) -> Result<(std::process::ExitStatus, String)> {
        use std::io::{BufRead, BufReader};
        use std::process::Stdio;
        let mut child = self
            .command(invocation)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to run smolvm")?;
        let stdout = child.stdout.take().expect("piped stdout");
        let echo = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                eprintln!("workmux:     {line}");
            }
        });
        let stderr = child.stderr.take().expect("piped stderr");
        let collect = std::thread::spawn(move || {
            let mut collected = String::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("workmux:     {line}");
                collected.push_str(&line);
                collected.push('\n');
            }
            collected
        });
        let status = wait_passing_on_hangup(&mut child, signals)?;
        echo.join().ok();
        Ok((status, collect.join().unwrap_or_default()))
    }
}

/// Wait for `child`, sending it SIGHUP (once) when `signals` catches one,
/// so it ends and this process lives on to clean up after it.
fn wait_passing_on_hangup(
    child: &mut std::process::Child,
    signals: &CaughtSignals,
) -> Result<std::process::ExitStatus> {
    let mut forwarded = false;
    // Short at first: most steps take milliseconds.
    let mut pause = Duration::from_millis(1);
    loop {
        if let Some(status) = child.try_wait().context("Failed to wait for smolvm")? {
            return Ok(status);
        }
        if !forwarded && signals.caught() {
            forwarded = true;
            info!("hung up; ending smolvm's call");
            // SAFETY: kill has no memory-safety preconditions.
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGHUP) };
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(50));
    }
}

/// A step's outcome: smolvm's error when it failed.
fn step_result(invocation: &Invocation, output: &std::process::Output) -> Result<()> {
    if !output.status.success() {
        bail!(
            "smolvm {} failed ({}): {}",
            invocation.args[..2].join(" "),
            output.status,
            smolvm_error(&output.stderr)
        );
    }
    Ok(())
}

/// smolvm's error from a failed call's stderr: `CODE: message` from its
/// JSON error (the last line that is one), else the stderr as it is.
fn smolvm_error(stderr: &[u8]) -> String {
    #[derive(serde::Deserialize)]
    struct Report {
        error: ReportedError,
    }
    #[derive(serde::Deserialize)]
    struct ReportedError {
        code: String,
        message: String,
    }
    let stderr = String::from_utf8_lossy(stderr);
    stderr
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<Report>(line.trim()).ok())
        .map(|report| format!("{}: {}", report.error.code, report.error.message))
        .unwrap_or_else(|| stderr.trim().to_string())
}

/// A failed tool's error from its stderr: smolvm's own error if it reported
/// one, else from the tool's last unindented `error` line on (Nix's and
/// Devbox's errors end their output, after any download progress), else
/// its last lines.
fn tool_error(stderr: &str) -> String {
    const MAX_LINES: usize = 20;
    let smolvm = smolvm_error(stderr.as_bytes());
    if smolvm != stderr.trim() {
        return smolvm;
    }
    let lines: Vec<&str> = stderr.trim().lines().collect();
    let from = lines
        .iter()
        .rposition(|line| line.to_ascii_lowercase().starts_with("error"))
        .unwrap_or(0)
        .max(lines.len().saturating_sub(MAX_LINES));
    lines[from..].join("\n")
}

/// Removes a pack build's context when dropped, whatever happened.
struct ContextGuard<'a>(&'a PackContext);

impl Drop for ContextGuard<'_> {
    fn drop(&mut self) {
        remove_pack_context(self.0);
    }
}

/// Deletes the pane's machine when dropped, after an error as after the exec.
struct MachineGuard<'a> {
    runner: &'a Runner,
    machine: &'a str,
    delete: &'a Invocation,
}

impl Drop for MachineGuard<'_> {
    fn drop(&mut self) {
        match self.runner.command(self.delete).output() {
            Ok(output) if output.status.success() => {
                debug!(machine = %self.machine, "machine deleted");
            }
            Ok(output) => warn!(
                machine = %self.machine,
                error = %smolvm_error(&output.stderr),
                "failed to delete machine"
            ),
            Err(error) => warn!(machine = %self.machine, %error, "failed to run smolvm delete"),
        }
    }
}

/// Records SIGHUP, SIGTERM and SIGINT instead of dying of them.
struct CaughtSignals {
    caught: Arc<AtomicBool>,
    ids: Vec<signal_hook::SigId>,
}

impl CaughtSignals {
    fn install() -> Result<Self> {
        let caught = Arc::new(AtomicBool::new(false));
        let mut ids = Vec::new();
        for signal in [libc::SIGHUP, libc::SIGTERM, libc::SIGINT] {
            ids.push(signal_hook::flag::register(signal, caught.clone())?);
        }
        Ok(Self { caught, ids })
    }

    fn caught(&self) -> bool {
        self.caught.load(Ordering::Relaxed)
    }

    /// Fail once a signal is caught, so whatever was created is deleted
    /// and nothing more is started.
    fn check(&self) -> Result<()> {
        if self.caught() {
            bail!("hung up or interrupted before the agent started");
        }
        Ok(())
    }
}

impl Drop for CaughtSignals {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::smolvm::pack::{
        BuildStep, ContextSource, PackBuild, PackContext, PackPlan, PackWhen,
    };
    use crate::sandbox::smolvm::plan::Step;
    use std::path::Path;

    /// A `smolvm` that refuses a call without `--error-format json` first,
    /// logs each call after it (and the RPC token it was given), logs the
    /// `FAKE_SMOLVM_CONTEXT_FILES` a create's `-v` mount holds, and fails
    /// the subcommand named in `FAKE_SMOLVM_FAIL` with `FAKE_SMOLVM_CODE`,
    /// reporting it as smolvm's JSON error. `@CAT@` and `@SLEEP@` stand for
    /// those tools' paths, found on the parent's `PATH` because the child's
    /// holds only the fake and a Nix build has no `/bin/cat` or `/bin/sleep`.
    const FAKE_SMOLVM: &str = r#"#!/bin/sh
if [ "$1" != --error-format ] || [ "$2" != json ]; then
    echo "fake smolvm: called without --error-format json: $*" >&2
    exit 97
fi
shift 2
echo "$* token=${WM_RPC_TOKEN:-}" >> "$FAKE_SMOLVM_LOG"
if [ -n "${GITHUB_TOKEN:-}" ]; then echo "$4 $GITHUB_TOKEN" >> "$FAKE_SMOLVM_LOG.env"; fi
if [ -n "${WM_GITHUB_GIT_AUTH:-}" ]; then echo "$4 git $WM_GITHUB_GIT_AUTH" >> "$FAKE_SMOLVM_LOG.env"; fi
if [ "$3" = machine ] && [ "$4" = create ] && [ -n "${FAKE_SMOLVM_CONTEXT_FILES:-}" ]; then
    prev=
    for word; do
        if [ "$prev" = -v ]; then
            for file in $FAKE_SMOLVM_CONTEXT_FILES; do
                @CAT@ "${word%%:*}/$file" >> "$FAKE_SMOLVM_LOG.context" 2>/dev/null
            done
        fi
        prev=$word
    done
fi
if [ "$3" = pack ]; then
    for out; do :; done
    : > "$out"
    echo pack > "$out.smolmachine"
fi
if [ "$4" = ls ]; then
    @CAT@ "$FAKE_SMOLVM_LS" 2>/dev/null || echo '[]'
    exit 0
fi
if [ -n "$FAKE_SMOLVM_HANG" ] && [ "$4" = "$FAKE_SMOLVM_HANG" ]; then
    trap 'echo "$4 got SIGHUP" >> "$FAKE_SMOLVM_LOG"; exit 129' HUP
    @SLEEP@ 30 >/dev/null 2>&1 &
    : > "$FAKE_SMOLVM_LOG.hanging"
    wait $!
    exit 0
fi
for word in "$@"; do
    if [ "$word" = "$FAKE_SMOLVM_FAIL" ]; then
        echo "Booting..." >&2
        echo "{\"error\":{\"code\":\"INVALID_STATE\",\"message\":\"smolvm says no to $word\"}}" >&2
        exit "$FAKE_SMOLVM_CODE"
    fi
done
exit 0
"#;

    fn invocation(args: &[&str]) -> Invocation {
        Invocation {
            args: args.iter().map(|a| a.to_string()).collect(),
            env: Vec::new(),
        }
    }

    /// A two-step pack build into `dir` (the fake `pack create` writes its
    /// output).
    fn pack(dir: &Path, when: PackWhen) -> PackPlan {
        let output = dir.join(".wm-pack-p.building");
        PackPlan {
            build: PackBuild {
                name: "wm-pack-p".into(),
                image: "alpine".into(),
                dir: dir.to_path_buf(),
                sidecar: dir.join("wm-pack-p.smolmachine"),
                lock: dir.join("wm-pack-p.lock"),
                current_marker: dir.join("current").join("slot"),
                steps: vec![
                    BuildStep {
                        label: "create the base machine".into(),
                        invocation: invocation(&["machine", "create", "--name", "wm-pack-p"]),
                        warm_up: false,
                    },
                    BuildStep {
                        label: "pack the base machine".into(),
                        invocation: invocation(&[
                            "pack",
                            "create",
                            "--from-vm",
                            "wm-pack-p",
                            "-o",
                            &output.to_string_lossy(),
                        ]),
                        warm_up: false,
                    },
                ],
                output,
                delete_base: invocation(&["machine", "delete", "--name", "wm-pack-p", "-f"]),
                context: None,
            },
            when,
        }
    }

    /// The test's pack index: a directory beside the fake's log.
    fn pack_dir(log: &Path) -> std::path::PathBuf {
        log.parent().unwrap().join("packs")
    }

    fn plan() -> LaunchPlan {
        plan_with_pack(pack(Path::new("/nonexistent-packs"), PackWhen::Present))
    }

    fn plan_with_pack(pack: PackPlan) -> LaunchPlan {
        LaunchPlan {
            machine: "wm-m".into(),
            pack,
            prepare: vec![
                Step {
                    kind: StepKind::CreateMachine,
                    invocation: invocation(&["machine", "create", "--name", "wm-m"]),
                },
                Step {
                    kind: StepKind::StartMachine,
                    invocation: invocation(&["machine", "start", "--name", "wm-m"]),
                },
            ],
            exec: Invocation {
                args: ["machine", "exec", "--name", "wm-m", "--", "claude"]
                    .map(String::from)
                    .to_vec(),
                env: vec![("WM_RPC_TOKEN".into(), "tok".into())],
            },
            delete: invocation(&["machine", "delete", "--name", "wm-m", "-f"]),
            notes: Vec::new(),
        }
    }

    /// Run `body` in a child test process whose `PATH` holds only the fake
    /// `smolvm`, failing `fail` (a subcommand word) with `code`.
    fn with_fake_smolvm(test: &str, fail: &str, code: &str, body: impl FnOnce(&Path)) {
        with_fake_smolvm_hanging(test, fail, code, "", body);
    }

    /// As [`with_fake_smolvm`], failing nothing, with `envs` set too.
    fn with_fake_smolvm_env(test: &str, envs: &[(&str, &Path)], body: impl FnOnce(&Path)) {
        with_fake_smolvm_inner(test, "", "0", "", envs, body);
    }

    fn with_fake_smolvm_hanging(
        test: &str,
        fail: &str,
        code: &str,
        hang: &str,
        body: impl FnOnce(&Path),
    ) {
        with_fake_smolvm_inner(test, fail, code, hang, &[], body);
    }

    fn with_fake_smolvm_inner(
        test: &str,
        fail: &str,
        code: &str,
        hang: &str,
        envs: &[(&str, &Path)],
        body: impl FnOnce(&Path),
    ) {
        if crate::test_support::is_isolated_child(test) {
            body(Path::new(&std::env::var_os("FAKE_SMOLVM_LOG").unwrap()));
            println!("{}", crate::test_support::ISOLATED_TEST_CANARY);
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let fake = bin.join("smolvm");
        let script = FAKE_SMOLVM
            .replace("@CAT@", &host_tool("cat"))
            .replace("@SLEEP@", &host_tool("sleep"));
        std::fs::write(&fake, script).unwrap();
        // The pack context is written with git and tar.
        for tool in ["git", "tar"] {
            std::os::unix::fs::symlink(host_tool(tool), bin.join(tool)).unwrap();
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let log = dir.path().join("log");
        let ls = dir.path().join("ls.json");
        let mut all_envs: Vec<(&str, &Path)> = envs.to_vec();
        all_envs.extend_from_slice(&[
            ("PATH", &bin),
            ("FAKE_SMOLVM_LOG", &log),
            ("FAKE_SMOLVM_FAIL", Path::new(fail)),
            ("FAKE_SMOLVM_CODE", Path::new(code)),
            ("FAKE_SMOLVM_HANG", Path::new(hang)),
            ("FAKE_SMOLVM_LS", &ls),
            (STATE_DIR_ENV, Path::new("/state")),
        ]);
        crate::test_support::run_isolated_test(test, dir.path(), &all_envs);
    }

    /// The path of `tool` on this process's `PATH`.
    fn host_tool(tool: &str) -> String {
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&path)
            .map(|dir| dir.join(tool))
            .find(|candidate| candidate.is_file())
            .unwrap_or_else(|| panic!("no {tool} on PATH"))
            .to_string_lossy()
            .into_owned()
    }

    fn calls(log: &Path) -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    #[test]
    fn test_runs_the_plan_and_deletes_the_machine_whatever_the_exec_exit_status() {
        const TEST: &str = "sandbox::smolvm::runner::tests::test_runs_the_plan_and_deletes_the_machine_whatever_the_exec_exit_status";
        with_fake_smolvm(TEST, "exec", "3", |log| {
            assert_eq!(Runner::from_env().run(&plan()).unwrap(), 3);
            assert_eq!(
                calls(log),
                [
                    "--state-dir /state machine create --name wm-m token=",
                    "--state-dir /state machine start --name wm-m token=",
                    "--state-dir /state machine exec --name wm-m -- claude token=tok",
                    "--state-dir /state machine delete --name wm-m -f token=",
                ]
            );
        });
    }

    #[test]
    fn test_credential_values_reach_smolvm_only_through_its_environment() {
        const TEST: &str = "sandbox::smolvm::runner::tests::test_credential_values_reach_smolvm_only_through_its_environment";
        const VALUE: &str = "ghp-throwaway-not-a-real-token";
        with_fake_smolvm_env(
            TEST,
            &[
                ("GITHUB_TOKEN", Path::new(VALUE)),
                ("EDITOR", Path::new(VALUE)),
            ],
            |log| {
                use crate::sandbox::smolvm::{credentials, plan};
                use std::sync::{Arc, Mutex};

                #[derive(Clone, Default)]
                struct Buffer(Arc<Mutex<Vec<u8>>>);
                impl std::io::Write for Buffer {
                    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                        self.0.lock().unwrap().extend_from_slice(bytes);
                        Ok(bytes.len())
                    }
                    fn flush(&mut self) -> std::io::Result<()> {
                        Ok(())
                    }
                }

                let sandbox: crate::config::SandboxConfig =
                    serde_yaml::from_str("env_passthrough: [GITHUB_TOKEN, EDITOR]\n").unwrap();
                let mut input = plan::tests::claude_input(&sandbox);
                input.host_env = credentials::host_env(&sandbox);
                input.github_tokens =
                    credentials::github_tokens(&sandbox, |var| std::env::var(var).ok());
                let mut launch = plan::plan_launch(&input).unwrap();
                // base64("x-access-token:ghp-throwaway-not-a-real-token")
                const DERIVED: &str =
                    "eC1hY2Nlc3MtdG9rZW46Z2hwLXRocm93YXdheS1ub3QtYS1yZWFsLXRva2Vu";
                let shown = format!("{launch:?}");
                assert!(!shown.contains(VALUE), "{shown}");
                assert!(!shown.contains(DERIVED), "{shown}");
                assert!(shown.contains("WM_GITHUB_GIT_AUTH"), "{shown}");
                launch.pack = pack(Path::new("/nonexistent-packs"), PackWhen::Present);

                let buffer = Buffer::default();
                let writer = buffer.clone();
                let subscriber = tracing_subscriber::fmt()
                    .with_max_level(tracing::Level::TRACE)
                    .with_writer(move || writer.clone())
                    .finish();
                tracing::subscriber::with_default(subscriber, || {
                    Runner::from_env().run(&launch).unwrap();
                });
                let logged = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
                assert!(logged.contains("--credential"), "{logged}");
                assert!(!logged.contains(VALUE), "{logged}");
                assert!(!logged.contains(DERIVED), "{logged}");

                let argv = std::fs::read_to_string(log).unwrap();
                assert!(
                    argv.contains("--secret-env GITHUB_TOKEN=GITHUB_TOKEN"),
                    "{argv}"
                );
                assert!(argv.contains("-e EDITOR "), "{argv}");
                assert!(
                    argv.contains(
                        "--credential WM_GITHUB_GIT_AUTH=WM_GITHUB_GIT_AUTH@github.com:set-header=Authorization"
                    ),
                    "{argv}"
                );
                assert!(!argv.contains(VALUE), "{argv}");
                assert!(!argv.contains(DERIVED), "{argv}");
                // smolvm itself got the values, from its environment; the
                // derived header only at start.
                let seen = std::fs::read_to_string(log.with_extension("env")).unwrap();
                assert!(seen.contains(&format!("start {VALUE}")), "{seen}");
                let git: Vec<&str> = seen.lines().filter(|line| line.contains(" git ")).collect();
                assert_eq!(git, [format!("start git Basic {DERIVED}")], "{seen}");
            },
        );
    }

    #[test]
    fn test_a_hangup_ends_the_exec_and_the_machine_is_still_deleted() {
        const TEST: &str = "sandbox::smolvm::runner::tests::test_a_hangup_ends_the_exec_and_the_machine_is_still_deleted";
        with_fake_smolvm_hanging(TEST, "", "0", "exec", |log| {
            hang_up_once_hanging(log);
            assert_eq!(Runner::from_env().run(&plan()).unwrap(), 129);
            let calls = calls(log);
            assert_eq!(calls[calls.len() - 2], "exec got SIGHUP", "{calls:?}");
            assert_eq!(
                calls.last().unwrap(),
                "--state-dir /state machine delete --name wm-m -f token="
            );
        });
    }

    /// Hang up on this process, as closing the pane does, once the fake
    /// has trapped the hangup and is hanging in the step under test.
    fn hang_up_once_hanging(log: &Path) {
        let hanging = log.with_extension("hanging");
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while !hanging.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            unsafe { libc::kill(libc::getpid(), libc::SIGHUP) };
        });
    }

    #[test]
    fn test_a_hangup_while_the_machine_starts_deletes_it_and_never_execs() {
        const TEST: &str = "sandbox::smolvm::runner::tests::test_a_hangup_while_the_machine_starts_deletes_it_and_never_execs";
        with_fake_smolvm_hanging(TEST, "", "0", "start", |log| {
            hang_up_once_hanging(log);
            let error = format!("{:#}", Runner::from_env().run(&plan()).unwrap_err());
            assert!(error.contains("hung up"), "{error}");
            assert_eq!(
                calls(log),
                [
                    "--state-dir /state machine create --name wm-m token=",
                    "--state-dir /state machine start --name wm-m token=",
                    "start got SIGHUP",
                    "--state-dir /state machine delete --name wm-m -f token=",
                ]
            );
        });
    }

    #[test]
    fn test_a_hangup_during_a_pack_build_deletes_its_base_and_creates_nothing_else() {
        const TEST: &str = "sandbox::smolvm::runner::tests::test_a_hangup_during_a_pack_build_deletes_its_base_and_creates_nothing_else";
        with_fake_smolvm_hanging(TEST, "", "0", "create", |log| {
            let dir = pack_dir(log);
            hang_up_once_hanging(log);
            let plan = plan_with_pack(pack(&dir, PackWhen::BuildFirst));
            let error = format!("{:#}", Runner::from_env().run(&plan).unwrap_err());
            assert!(error.contains("hung up"), "{error}");
            assert_eq!(
                calls(log),
                [
                    "--state-dir /state machine delete --name wm-pack-p -f token=",
                    "--state-dir /state machine create --name wm-pack-p token=",
                    "create got SIGHUP",
                    "--state-dir /state machine delete --name wm-pack-p -f token=",
                ]
            );
            assert!(!dir.join("wm-pack-p.smolmachine").exists());
        });
    }

    #[test]
    fn test_a_hangup_after_a_pack_build_deletes_the_machine_and_keeps_the_pack() {
        const TEST: &str = "sandbox::smolvm::runner::tests::test_a_hangup_after_a_pack_build_deletes_the_machine_and_keeps_the_pack";
        with_fake_smolvm_hanging(TEST, "", "0", "start", |log| {
            let dir = pack_dir(log);
            hang_up_once_hanging(log);
            let plan = plan_with_pack(pack(&dir, PackWhen::BuildFirst));
            let error = format!("{:#}", Runner::from_env().run(&plan).unwrap_err());
            assert!(error.contains("hung up"), "{error}");
            let calls = calls(log);
            assert_eq!(
                calls[calls.len() - 4..],
                [
                    "--state-dir /state machine create --name wm-m token=",
                    "--state-dir /state machine start --name wm-m token=",
                    "start got SIGHUP",
                    "--state-dir /state machine delete --name wm-m -f token=",
                ]
            );
            assert!(dir.join("wm-pack-p.smolmachine").is_file());
        });
    }

    #[test]
    fn test_a_launch_first_deletes_machines_whose_owner_is_gone() {
        const TEST: &str = "sandbox::smolvm::runner::tests::test_a_launch_first_deletes_machines_whose_owner_is_gone";
        with_fake_smolvm(TEST, "", "0", |log| {
            use crate::config::SandboxConfig;
            use crate::sandbox::smolvm::machines::Owner;
            let me = Owner::current().unwrap();
            let reused = Owner {
                start: me.start + 1,
                ..me
            };
            let ls = serde_json::json!([
                {"name": "plain", "state": "running", "labels": {}},
                {"name": "wm-live", "state": "running",
                 "labels": {"workmux": "1", "wm-owner": me.to_string()}},
                {"name": "wm-dead", "state": "running",
                 "labels": {"workmux": "1", "wm-owner": "2147483646:1"}},
                {"name": "wm-reused", "state": "stopped",
                 "labels": {"workmux": "1", "wm-owner": reused.to_string()}},
            ]);
            let ls_path = std::env::var_os("FAKE_SMOLVM_LS").unwrap();
            std::fs::write(ls_path, ls.to_string()).unwrap();

            let sandbox = SandboxConfig::default();
            let mut input = crate::sandbox::smolvm::plan::tests::claude_input(&sandbox);
            // The pack is present: this launch builds nothing.
            input.pack.dir = pack_dir(log);
            let pack = crate::sandbox::smolvm::plan::plan_launch(&input)
                .unwrap()
                .pack;
            input.pack.present = vec![pack.build.name];
            assert_eq!(
                crate::sandbox::smolvm::launch(&Runner::from_env(), input).unwrap(),
                0
            );
            // Each call as its subcommand and machine.
            let names: Vec<String> = calls(log)
                .iter()
                .map(|call| {
                    let words: Vec<&str> = call.split(' ').collect();
                    match words.iter().position(|w| *w == "--name") {
                        Some(i) => format!("{} {}", words[3], words[i + 1]),
                        None => words[3].to_string(),
                    }
                })
                .collect();
            assert_eq!(
                names,
                [
                    "ls",
                    "delete wm-dead",
                    "delete wm-reused",
                    "create wm-feat-42",
                    "start wm-feat-42",
                    "exec wm-feat-42",
                    "delete wm-feat-42",
                ]
            );
        });
    }

    #[test]
    fn test_removing_a_worktree_deletes_its_machines_and_only_its_own() {
        const TEST: &str = "sandbox::smolvm::runner::tests::test_removing_a_worktree_deletes_its_machines_and_only_its_own";
        with_fake_smolvm(TEST, "wm-b", "1", |log| {
            use crate::sandbox::smolvm::machines::path_hash;
            let mine = path_hash(Path::new("/src/p__worktrees/feat"));
            let other = path_hash(Path::new("/src/p__worktrees/other"));
            let ls = serde_json::json!([
                {"name": "wm-a", "state": "running",
                 "labels": {"workmux": "1", "wm-worktree": mine, "wm-owner": "1:1"}},
                {"name": "wm-b", "state": "stopped",
                 "labels": {"workmux": "1", "wm-worktree": mine}},
                {"name": "wm-c", "state": "running",
                 "labels": {"workmux": "1", "wm-worktree": mine}},
                {"name": "wm-other", "state": "running",
                 "labels": {"workmux": "1", "wm-worktree": other}},
                {"name": "not-ours", "state": "running",
                 "labels": {"wm-worktree": mine}},
            ]);
            std::fs::write(std::env::var_os("FAKE_SMOLVM_LS").unwrap(), ls.to_string()).unwrap();

            crate::sandbox::smolvm::delete_worktree_machines(
                &Runner::from_env(),
                Path::new("/src/p__worktrees/feat"),
            );
            assert_eq!(
                calls(log),
                [
                    "--state-dir /state machine ls --json token=",
                    "--state-dir /state machine delete --name wm-a -f token=",
                    "--state-dir /state machine delete --name wm-b -f token=",
                    "--state-dir /state machine delete --name wm-c -f token=",
                ]
            );
        });
    }

    #[test]
    fn test_a_failed_orphan_delete_is_logged_and_the_launch_goes_on() {
        const TEST: &str = "sandbox::smolvm::runner::tests::test_a_failed_orphan_delete_is_logged_and_the_launch_goes_on";
        with_fake_smolvm(TEST, "wm-gone", "1", |log| {
            let mut plan = plan();
            plan.prepare.insert(
                0,
                Step {
                    kind: StepKind::DeleteOrphan,
                    invocation: invocation(&["machine", "delete", "--name", "wm-gone", "-f"]),
                },
            );
            assert_eq!(Runner::from_env().run(&plan).unwrap(), 0);
            assert_eq!(
                calls(log),
                [
                    "--state-dir /state machine delete --name wm-gone -f token=",
                    "--state-dir /state machine create --name wm-m token=",
                    "--state-dir /state machine start --name wm-m token=",
                    "--state-dir /state machine exec --name wm-m -- claude token=tok",
                    "--state-dir /state machine delete --name wm-m -f token=",
                ]
            );
        });
    }

    #[test]
    fn test_a_failed_create_fails_closed_with_smolvms_error_and_never_execs() {
        const TEST: &str = "sandbox::smolvm::runner::tests::test_a_failed_create_fails_closed_with_smolvms_error_and_never_execs";
        with_fake_smolvm(TEST, "create", "1", |log| {
            let dir = pack_dir(log);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("wm-pack-p.smolmachine"), "old").unwrap();
            let plan = plan_with_pack(pack(&dir, PackWhen::Present));
            let error = format!("{:#}", Runner::from_env().run(&plan).unwrap_err());
            assert!(error.contains("smolvm says no to create"), "{error}");
            let calls = calls(log);
            assert!(!calls.iter().any(|c| c.contains(" exec ")), "{calls:?}");
            assert_eq!(
                calls[0],
                "--state-dir /state machine create --name wm-m token="
            );
        });
    }

    #[test]
    fn test_a_failed_start_fails_the_launch_with_smolvms_error_and_deletes_the_machine() {
        const TEST: &str = "sandbox::smolvm::runner::tests::test_a_failed_start_fails_the_launch_with_smolvms_error_and_deletes_the_machine";
        with_fake_smolvm(TEST, "start", "1", |log| {
            let error = format!("{:#}", Runner::from_env().run(&plan()).unwrap_err());
            assert!(error.contains("smolvm says no to start"), "{error}");
            assert_eq!(
                calls(log),
                [
                    "--state-dir /state machine create --name wm-m token=",
                    "--state-dir /state machine start --name wm-m token=",
                    "--state-dir /state machine delete --name wm-m -f token=",
                ]
            );
        });
    }

    #[test]
    fn test_a_failed_step_reports_smolvms_error_code_and_message() {
        const TEST: &str = "sandbox::smolvm::runner::tests::test_a_failed_step_reports_smolvms_error_code_and_message";
        with_fake_smolvm(TEST, "start", "1", |_| {
            let error = format!("{:#}", Runner::from_env().run(&plan()).unwrap_err());
            assert!(
                error.contains("INVALID_STATE: smolvm says no to start"),
                "{error}"
            );
            assert!(!error.contains("{\"error\""), "{error}");
        });
    }

    #[test]
    fn test_a_failed_hot_mount_fails_the_launch_and_deletes_the_machine() {
        const TEST: &str = "sandbox::smolvm::runner::tests::test_a_failed_hot_mount_fails_the_launch_and_deletes_the_machine";
        with_fake_smolvm(TEST, "mount", "1", |log| {
            let mut plan = plan();
            plan.prepare.push(Step {
                kind: StepKind::MountDirectory,
                invocation: invocation(&[
                    "machine", "mount", "add", "--name", "wm-m", "-v", "/n:/n:ro",
                ]),
            });
            let error = format!("{:#}", Runner::from_env().run(&plan).unwrap_err());
            assert!(error.contains("smolvm says no to mount"), "{error}");
            assert_eq!(
                calls(log),
                [
                    "--state-dir /state machine create --name wm-m token=",
                    "--state-dir /state machine start --name wm-m token=",
                    "--state-dir /state machine mount add --name wm-m -v /n:/n:ro token=",
                    "--state-dir /state machine delete --name wm-m -f token=",
                ]
            );
        });
    }

    #[test]
    fn test_a_missing_pack_is_built_after_the_orphans_and_before_the_machine() {
        const TEST: &str = "sandbox::smolvm::runner::tests::test_a_missing_pack_is_built_after_the_orphans_and_before_the_machine";
        with_fake_smolvm(TEST, "", "0", |log| {
            let dir = pack_dir(log);
            let mut plan = plan_with_pack(pack(&dir, PackWhen::BuildFirst));
            plan.prepare.insert(
                0,
                Step {
                    kind: StepKind::DeleteOrphan,
                    invocation: invocation(&["machine", "delete", "--name", "wm-gone", "-f"]),
                },
            );
            assert_eq!(Runner::from_env().run(&plan).unwrap(), 0);
            let out = dir.join(".wm-pack-p.building");
            assert_eq!(
                calls(log),
                [
                    "--state-dir /state machine delete --name wm-gone -f token=".to_string(),
                    // A base left by a killed build goes first.
                    "--state-dir /state machine delete --name wm-pack-p -f token=".to_string(),
                    "--state-dir /state machine create --name wm-pack-p token=".to_string(),
                    format!(
                        "--state-dir /state pack create --from-vm wm-pack-p -o {} token=",
                        out.display()
                    ),
                    "--state-dir /state machine delete --name wm-pack-p -f token=".to_string(),
                    "--state-dir /state machine create --name wm-m token=".to_string(),
                    "--state-dir /state machine start --name wm-m token=".to_string(),
                    "--state-dir /state machine exec --name wm-m -- claude token=tok".to_string(),
                    "--state-dir /state machine delete --name wm-m -f token=".to_string(),
                ]
            );
            // The sidecar moved into place, smolvm's stub executable gone, and
            // the pack recorded as its image's current one.
            assert!(dir.join("wm-pack-p.smolmachine").is_file());
            assert!(!out.exists());
            assert_eq!(
                std::fs::read_to_string(dir.join("current/slot")).unwrap(),
                "wm-pack-p\n"
            );
        });
    }

    #[test]
    fn test_a_pack_built_while_waiting_for_the_lock_is_not_built_again() {
        const TEST: &str = "sandbox::smolvm::runner::tests::test_a_pack_built_while_waiting_for_the_lock_is_not_built_again";
        with_fake_smolvm(TEST, "", "0", |log| {
            let dir = pack_dir(log);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("wm-pack-p.smolmachine"), "pack").unwrap();
            let plan = plan_with_pack(pack(&dir, PackWhen::BuildFirst));
            assert_eq!(Runner::from_env().run(&plan).unwrap(), 0);
            let calls = calls(log);
            assert!(!calls.iter().any(|c| c.contains("wm-pack-p")), "{calls:?}");
        });
    }

    #[test]
    fn test_a_failed_build_deletes_the_base_and_never_creates_the_machine() {
        const TEST: &str = "sandbox::smolvm::runner::tests::test_a_failed_build_deletes_the_base_and_never_creates_the_machine";
        with_fake_smolvm(TEST, "pack", "1", |log| {
            let dir = pack_dir(log);
            let plan = plan_with_pack(pack(&dir, PackWhen::BuildFirst));
            let error = format!("{:#}", Runner::from_env().run(&plan).unwrap_err());
            assert!(error.contains("pack the base machine"), "{error}");
            assert!(error.contains("smolvm says no to pack"), "{error}");
            let calls = calls(log);
            assert_eq!(
                calls.last().unwrap(),
                "--state-dir /state machine delete --name wm-pack-p -f token="
            );
            assert!(!calls.iter().any(|c| c.contains("wm-m")), "{calls:?}");
            assert!(!dir.join("wm-pack-p.smolmachine").exists());
            assert!(!dir.join("current/slot").exists());
        });
    }

    /// A git repository at `dir` whose one commit tracks `flake.nix` and
    /// `nix/shortcuts.nix`, with `scratch.txt` untracked; returns the commit.
    fn repo_with_imports(dir: &Path) -> String {
        std::fs::create_dir_all(dir.join("nix")).unwrap();
        std::fs::write(
            dir.join("flake.nix"),
            "{ imports = [ ./nix/shortcuts.nix ]; }\n",
        )
        .unwrap();
        std::fs::write(dir.join("nix/shortcuts.nix"), "shortcuts\n").unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?}: {output:?}");
            String::from_utf8(output.stdout).unwrap()
        };
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);
        std::fs::write(dir.join("scratch.txt"), "untracked\n").unwrap();
        git(&["rev-parse", "HEAD"]).trim().to_string()
    }

    /// [`pack`] with a pack context of `commit` in `repo`, mounted at create,
    /// and a warm-up step between the create and the pack.
    fn pack_with_context(dir: &Path, repo: &Path, commit: &str) -> PackPlan {
        let mut plan = pack(dir, PackWhen::BuildFirst);
        let staging = dir.join(".wm-pack-p.context");
        plan.build.steps[0].invocation.args.extend([
            "-v".to_string(),
            format!("{}:/var/lib/workmux/pack-context:ro", staging.display()),
        ]);
        plan.build.steps.insert(
            1,
            BuildStep {
                label: "realise the default branch's nix develop shell".into(),
                invocation: invocation(&["machine", "exec", "--name", "wm-pack-p", "--", "warm"]),
                warm_up: true,
            },
        );
        plan.build.context = Some(PackContext {
            source: ContextSource {
                repo: repo.to_path_buf(),
                commit: commit.to_string(),
            },
            staging,
        });
        plan
    }

    #[test]
    fn test_a_pack_build_mounts_the_commits_tracked_files_and_removes_them_after() {
        const TEST: &str = "sandbox::smolvm::runner::tests::test_a_pack_build_mounts_the_commits_tracked_files_and_removes_them_after";
        let files = Path::new("nix/shortcuts.nix scratch.txt");
        with_fake_smolvm_env(TEST, &[("FAKE_SMOLVM_CONTEXT_FILES", files)], |log| {
            let dir = pack_dir(log);
            let repo = log.parent().unwrap().join("repo");
            let commit = repo_with_imports(&repo);
            let plan = pack_with_context(&dir, &repo, &commit);
            let warning = Runner::from_env().build_pack(&plan.build).unwrap();
            assert_eq!(warning, None, "the shell is warm");
            assert_eq!(
                std::fs::read_to_string(log.with_extension("context")).unwrap(),
                "shortcuts\n",
                "the imported file is mounted, the untracked one isn't"
            );
            assert!(dir.join("wm-pack-p.smolmachine").is_file());
            assert!(!dir.join(".wm-pack-p.context").exists());
        });
    }

    #[test]
    fn test_a_failed_pack_build_removes_its_pack_context_too() {
        const TEST: &str =
            "sandbox::smolvm::runner::tests::test_a_failed_pack_build_removes_its_pack_context_too";
        with_fake_smolvm(TEST, "pack", "1", |log| {
            let dir = pack_dir(log);
            let repo = log.parent().unwrap().join("repo");
            let commit = repo_with_imports(&repo);
            let plan = pack_with_context(&dir, &repo, &commit);
            Runner::from_env().build_pack(&plan.build).unwrap_err();
            assert!(!dir.join(".wm-pack-p.context").exists());
            assert!(!dir.join("wm-pack-p.smolmachine").exists());
        });
    }

    #[test]
    fn test_a_failed_warm_up_still_packs_and_warns_with_the_error_and_how_to_rebuild() {
        const TEST: &str = "sandbox::smolvm::runner::tests::test_a_failed_warm_up_still_packs_and_warns_with_the_error_and_how_to_rebuild";
        with_fake_smolvm(TEST, "warm", "1", |log| {
            let dir = pack_dir(log);
            let repo = log.parent().unwrap().join("repo");
            let commit = repo_with_imports(&repo);
            let plan = pack_with_context(&dir, &repo, &commit);
            let warning = Runner::from_env()
                .build_pack(&plan.build)
                .unwrap()
                .expect("a cold pack warns");
            for part in [
                "smolvm says no to warm",
                "isn't warm",
                "workmux sandbox build",
                "workmux sandbox pull",
            ] {
                assert!(warning.contains(part), "{part}: {warning}");
            }
            assert!(dir.join("wm-pack-p.smolmachine").is_file());
            assert_eq!(
                std::fs::read_to_string(dir.join("current/slot")).unwrap(),
                "wm-pack-p\n"
            );
            assert!(!dir.join(".wm-pack-p.context").exists());
        });
    }

    #[test]
    fn test_a_warm_ups_error_is_the_tools_last_error_after_its_progress() {
        let nix = "copying path '/nix/store/a' from 'https://cache.nixos.org'...\n\
                   copying path '/nix/store/b' from 'https://cache.nixos.org'...\n\
                   error:\n\
                   \x20      … while evaluating the flake\n\
                   \x20      error: path '/tmp/workmux-pack-context/nix/x.nix' does not exist\n";
        assert_eq!(
            tool_error(nix),
            "error:\n       … while evaluating the flake\n       \
             error: path '/tmp/workmux-pack-context/nix/x.nix' does not exist"
        );
        let smolvm =
            "Booting...\n{\"error\":{\"code\":\"NOT_FOUND\",\"message\":\"no machine\"}}\n";
        assert_eq!(tool_error(smolvm), "NOT_FOUND: no machine");
        let quiet: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        assert!(
            tool_error(&quiet).starts_with("line 11\n"),
            "the last 20 lines"
        );
    }

    #[test]
    fn test_a_pack_context_that_cant_be_written_skips_the_warm_up_with_a_warning() {
        const TEST: &str = "sandbox::smolvm::runner::tests::test_a_pack_context_that_cant_be_written_skips_the_warm_up_with_a_warning";
        with_fake_smolvm(TEST, "", "0", |log| {
            let dir = pack_dir(log);
            let repo = log.parent().unwrap().join("repo");
            repo_with_imports(&repo);
            let plan = pack_with_context(&dir, &repo, "0000000000000000000000000000000000000000");
            let warning = Runner::from_env()
                .build_pack(&plan.build)
                .unwrap()
                .expect("a cold pack warns");
            assert!(warning.contains("isn't warm"), "{warning}");
            assert!(warning.contains("0000000"), "names the commit: {warning}");
            let calls = calls(log);
            assert!(!calls.iter().any(|c| c.contains(" warm ")), "{calls:?}");
            assert!(dir.join("wm-pack-p.smolmachine").is_file());
            assert!(!dir.join(".wm-pack-p.context").exists());
        });
    }

    #[test]
    fn test_a_superseded_pack_is_deleted_once_no_machine_from_it_remains() {
        const TEST: &str = "sandbox::smolvm::runner::tests::test_a_superseded_pack_is_deleted_once_no_machine_from_it_remains";
        with_fake_smolvm(TEST, "", "0", |log| {
            let dir = pack_dir(log);
            std::fs::create_dir_all(dir.join("current")).unwrap();
            for name in ["wm-pack-p-new", "wm-pack-p-used", "wm-pack-p-old"] {
                std::fs::write(dir.join(format!("{name}.smolmachine")), "").unwrap();
                std::fs::write(dir.join(format!("{name}.lock")), "").unwrap();
            }
            std::fs::write(dir.join("current/slot"), "wm-pack-p-new\n").unwrap();
            let ls = serde_json::json!([
                {"name": "wm-a", "state": "running",
                 "labels": {"workmux": "1", "wm-pack": "wm-pack-p-used"}},
            ]);
            std::fs::write(std::env::var_os("FAKE_SMOLVM_LS").unwrap(), ls.to_string()).unwrap();

            crate::sandbox::smolvm::sweep_superseded_packs(&Runner::from_env(), &dir);
            assert_eq!(
                crate::sandbox::smolvm::pack::present_packs(&dir),
                ["wm-pack-p-new", "wm-pack-p-used"]
            );
            assert!(!dir.join("wm-pack-p-old.lock").exists());
        });
    }
}
