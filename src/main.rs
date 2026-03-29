mod docker;
mod mounts;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "claudecage")]
/// Run Claude Code with full permissions inside a sandboxed Docker container.
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Arguments forwarded to claude.
    #[arg(last = true)]
    claude_args: Vec<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Manage the claudecage container.
    Container {
        #[command(subcommand)]
        action: ContainerAction,
    },
}

#[derive(Subcommand)]
enum ContainerAction {
    /// Build the Docker image and create the container.
    Init {
        /// Rebuild the image even if it already exists.
        #[arg(long)]
        rebuild: bool,
    },
    /// Update packages and claude inside the container.
    Refresh,
    /// Run the Claude subscription auth flow inside the container.
    Auth,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Some(Command::Container { action }) => match action {
            ContainerAction::Init { rebuild: _rebuild } => {
                anyhow::bail!("not implemented");
            }
            ContainerAction::Refresh => {
                anyhow::bail!("not implemented");
            }
            ContainerAction::Auth => {
                anyhow::bail!("not implemented");
            }
        },
        None => {
            anyhow::bail!("not implemented");
        }
    }
}
