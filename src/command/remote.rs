//! `workmux remote`: the internal commands workstation panes and transports
//! run (attaching to remote panes, serving the control channel, carrying
//! work across a rebuild). The sandbox's lifecycle is under `workmux sandbox`.

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use crate::remote::pane::{RemotePaneId, SessionHolder};

#[derive(Debug, Args)]
pub struct RemoteArgs {
    #[command(subcommand)]
    pub command: RemoteCommand,
}

#[derive(Debug, Subcommand)]
pub enum RemoteCommand {
    /// Show a remote pane in this terminal, reattaching after drops (run by workstation panes)
    #[command(hide = true)]
    Attach {
        /// Remote the pane runs on
        remote: String,
        /// Remote pane, as <project>/<worktree>/<slot>
        pane: RemotePaneId,
    },
    /// Serve the control channel on stdin and stdout (run by transports)
    #[command(hide = true)]
    Serve,
    /// Attach this terminal to a remote pane on this machine (run through transports)
    #[command(hide = true)]
    PaneAttach { pane: RemotePaneId },
    /// Print whether a remote pane on this machine is running or ended
    #[command(hide = true)]
    PaneState { pane: RemotePaneId },
    /// Carry a project's committed work across a rebuild (run through transports)
    #[command(hide = true)]
    Carry {
        /// at-risk, manifest, bundle or restore
        action: String,
        /// The project's clone
        path: String,
        /// For restore: the manifest, as JSON
        manifest: Option<String>,
    },
    /// Forward one multiplexer request from a remote shell to the workstation (run by deferred scripts)
    #[command(hide = true)]
    Forward {
        /// The request, as JSON
        request: String,
    },
}

pub fn run(args: RemoteArgs) -> Result<()> {
    match args.command {
        RemoteCommand::Attach { remote, pane } => crate::remote::attach::run(&remote, &pane),
        RemoteCommand::Serve => crate::remote::serve::run(),
        RemoteCommand::PaneAttach { pane } => {
            use std::os::unix::process::CommandExt;
            let error = SessionHolder::for_this_remote()?
                .attach_command(&pane)
                .exec();
            Err(error).context("Failed to run the session holder dtach")
        }
        RemoteCommand::PaneState { pane } => {
            println!(
                "{}",
                SessionHolder::for_this_remote()?.state(&pane)?.as_str()
            );
            Ok(())
        }
        RemoteCommand::Carry {
            action,
            path,
            manifest,
        } => crate::remote::carry::serve(&action, &path, manifest.as_deref()),
        RemoteCommand::Forward { request } => {
            let mux = crate::remote::forwarding::ForwardingMux::from_env()
                .context("Remote forward runs only in a remote shell")?;
            mux.forward(serde_json::from_str(&request).context("Unreadable request")?)
        }
    }
}
