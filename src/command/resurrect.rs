use anyhow::{Result, anyhow};
use tracing::info;

use crate::config;
use crate::multiplexer::{create_backend, detect_backend};
use crate::state::StateStore;
use crate::workflow::resurrect::{ResurrectAction, plan};
use crate::workflow::{self, SetupOptions, WorkflowContext};

pub fn run(dry_run: bool) -> Result<()> {
    if crate::remote::session::ServedProject::current().is_some() {
        return run_served(dry_run);
    }
    let local = run_local(dry_run);
    let remote_failures = resurrect_remote_projects(dry_run)?;
    local?;
    if remote_failures > 0 {
        return Err(anyhow!(
            "Failed to restore {} remote project(s)",
            remote_failures
        ));
    }
    Ok(())
}

/// Resurrect each remote project's worktrees on its remote, printing what
/// it restored under the project's name. Returns how many failed.
fn resurrect_remote_projects(dry_run: bool) -> Result<usize> {
    let mut failures = 0;
    for project in crate::remote::router::remote_projects()? {
        let mut argv = vec!["workmux", "resurrect"];
        if dry_run {
            argv.push("--dry-run");
        }
        let (name, remote) = (&project.site.name, &project.site.remote);
        match crate::remote::router::capture(&project, &argv) {
            Ok((code, stdout)) => {
                if !stdout.is_empty() {
                    println!("\n{name} (on {remote}):");
                    print!("{}", String::from_utf8_lossy(&stdout));
                }
                if code != 0 {
                    failures += 1;
                }
            }
            Err(error) => {
                eprintln!("  Failed to reach remote project '{name}' on {remote}: {error:#}");
                failures += 1;
            }
        }
    }
    Ok(failures)
}

/// Resurrect, on a remote, the windows of the served project's worktrees
/// whose remote panes survive unshown: each reopens showing them.
fn run_served(dry_run: bool) -> Result<()> {
    let surviving = crate::remote::forwarding::ForwardingMux::from_env()
        .ok_or_else(|| anyhow!("workstation not connected"))?
        .surviving_panes()?;
    let mut handles: Vec<String> = surviving.into_iter().map(|pane| pane.worktree).collect();
    handles.sort();
    handles.dedup();
    if handles.is_empty() {
        return Ok(());
    }
    if dry_run {
        for handle in &handles {
            println!("  {:<20} -> restoring", handle);
        }
        println!("\nDry run: would restore {} worktree(s)", handles.len());
        return Ok(());
    }

    let config = config::Config::load(None)?;
    let context = WorkflowContext::new(config, create_backend(detect_backend()), None)?;
    let mut restored = Vec::new();
    let mut failed = Vec::new();
    for handle in &handles {
        let mut options = SetupOptions::new(false, false, true);
        options.focus_window = false;
        options.mode = context.config.mode();
        options.resume_mode = crate::multiplexer::types::ResumeMode::Continue;
        match workflow::open(handle, &context, options, false, None, None, None) {
            Ok(result) if result.did_switch => {
                println!("  {:<20} -> skipping (already open)", handle);
            }
            Ok(_) => {
                println!("  {:<20} -> restoring", handle);
                restored.push(handle.clone());
            }
            Err(e) => {
                eprintln!("  Failed to restore '{}': {}", handle, e);
                failed.push(handle.clone());
            }
        }
    }
    summarize(&restored, &failed)
}

fn summarize(restored: &[String], failed: &[String]) -> Result<()> {
    if !restored.is_empty() {
        println!(
            "\n✓ Restored {} worktree(s): {}",
            restored.len(),
            restored.join(", ")
        );
    }
    if !failed.is_empty() {
        eprintln!(
            "✗ Failed to restore {} worktree(s): {}",
            failed.len(),
            failed.join(", ")
        );
        return Err(anyhow!("Failed to restore {} worktree(s)", failed.len()));
    }
    Ok(())
}

fn run_local(dry_run: bool) -> Result<()> {
    let config = config::Config::load(None)?;
    let mux = create_backend(detect_backend());
    let store = StateStore::new()?;

    let plan = plan(&store, mux.as_ref())?;

    if plan.candidates.is_empty() && plan.unmatched_states == 0 {
        println!("No agent state files found. Nothing to restore.");
        return Ok(());
    }

    // Print plan
    let to_restore: Vec<_> = plan
        .candidates
        .iter()
        .filter(|c| matches!(c.action, ResurrectAction::Restore))
        .collect();

    for candidate in &plan.candidates {
        let status = match &candidate.action {
            ResurrectAction::Restore => "restoring",
            ResurrectAction::SkipAlreadyOpen => "skipping (already open)",
            ResurrectAction::SkipMain => "skipping (main worktree)",
        };
        println!("  {:<20} -> {}", candidate.handle, status);
    }

    if plan.unmatched_states > 0 {
        println!(
            "  ({} unmatched state file(s) ignored)",
            plan.unmatched_states
        );
    }

    if to_restore.is_empty() {
        println!("\nNothing to restore.");
        return Ok(());
    }

    if dry_run {
        println!("\nDry run: would restore {} worktree(s)", to_restore.len());
        return Ok(());
    }

    // Execute restoration
    let context = WorkflowContext::new(config, mux, None)?;
    let mut restored = Vec::new();
    let mut failed = Vec::new();

    for candidate in &plan.candidates {
        if !matches!(candidate.action, ResurrectAction::Restore) {
            continue;
        }

        let options = SetupOptions {
            run_hooks: false,
            run_file_ops: false,
            run_pane_commands: true,
            prompt_file_path: None,
            focus_window: false,
            working_dir: None,
            config_root: None,
            open_if_exists: false,
            mode: candidate.mode,
            target_window_name: None,
            target_session_name: None,
            window_session_name: None,
            window_token: None,
            primary_window: true,
            resume_mode: crate::multiplexer::types::ResumeMode::Continue,
        };

        info!(
            handle = candidate.handle,
            mode = ?candidate.mode,
            stale_keys = candidate.stale_sources.len(),
            "resurrect:exec opening worktree"
        );

        match workflow::open(
            &candidate.handle,
            &context,
            options,
            false,
            None,
            None,
            candidate.agent.as_deref(),
        ) {
            Ok(result) => {
                info!(
                    handle = candidate.handle,
                    resolved = result.resolved_handle,
                    branch = result.branch_name,
                    path = %result.worktree_path.display(),
                    "resurrect:exec restored successfully"
                );
                if let Err(error) = store.consume_agent_sources(&candidate.stale_sources) {
                    tracing::warn!(%error, "resurrect:exec failed to consume stale state");
                }
                restored.push(candidate.handle.clone());
            }
            Err(e) => {
                info!(
                    handle = candidate.handle,
                    error = %e,
                    "resurrect:exec failed to restore"
                );
                eprintln!("  Failed to restore '{}': {}", candidate.handle, e);
                failed.push(candidate.handle.clone());
            }
        }
    }

    summarize(&restored, &failed)
}
