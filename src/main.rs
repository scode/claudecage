mod docker;
mod mounts;

use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use tracing::debug;

#[derive(Parser)]
#[command(name = "claudecage")]
/// Run Claude Code with full permissions inside a sandboxed Docker container.
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Increase log verbosity (repeat for more: -v debug, -vv trace).
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// Decrease log verbosity (repeat for less: -q warn, -qq error, -qqq off).
    #[arg(short = 'q', long = "quiet", action = clap::ArgAction::Count, global = true)]
    quiet: u8,

    /// Arguments forwarded to claude (after --).
    #[arg(last = true)]
    claude_args: Vec<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Manage the claudecage Docker image.
    Image {
        #[command(subcommand)]
        action: ImageAction,
    },
}

#[derive(Subcommand)]
enum ImageAction {
    /// Build the Docker image.
    Create {
        /// Rebuild the image even if it already exists.
        #[arg(long)]
        rebuild: bool,
    },
    /// Rebuild the image from scratch (no cache).
    Recreate,
}

fn log_level(verbose: u8, quiet: u8) -> tracing::level_filters::LevelFilter {
    const LEVELS: &[tracing::level_filters::LevelFilter] = &[
        tracing::level_filters::LevelFilter::OFF,
        tracing::level_filters::LevelFilter::ERROR,
        tracing::level_filters::LevelFilter::WARN,
        tracing::level_filters::LevelFilter::INFO,
        tracing::level_filters::LevelFilter::DEBUG,
        tracing::level_filters::LevelFilter::TRACE,
    ];
    let base: i8 = 3; // INFO
    let idx = (base + verbose as i8 - quiet as i8).clamp(0, LEVELS.len() as i8 - 1);
    LEVELS[idx as usize]
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_max_level(log_level(cli.verbose, cli.quiet))
        .init();

    let home = dirs::home_dir().context("could not determine home directory")?;

    match cli.command {
        Some(Command::Image { action }) => {
            match action {
                ImageAction::Create { rebuild } => {
                    if rebuild || !docker::image_exists()? {
                        docker::build_image(false)?;
                    }
                }
                ImageAction::Recreate => {
                    docker::build_image(true)?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        None => {
            if !docker::image_exists()? {
                bail!("image not found — run 'claudecage image create' first");
            }
            let workdir =
                std::env::current_dir().context("could not determine working directory")?;
            if !workdir.starts_with(&home) {
                bail!(
                    "working directory {} is outside the home directory — \
                     only projects under $HOME are accessible in the container",
                    workdir.display()
                );
            }
            let mounts = mounts::resolve_mounts(&home, &workdir)?;
            debug!(count = mounts.len(), "resolved mounts");
            docker::run_claude(&mounts, &workdir, &cli.claude_args)
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::level_filters::LevelFilter;

    #[test]
    fn log_level_defaults_to_info() {
        assert_eq!(log_level(0, 0), LevelFilter::INFO);
    }

    #[test]
    fn log_level_verbose_increases() {
        assert_eq!(log_level(1, 0), LevelFilter::DEBUG);
        assert_eq!(log_level(2, 0), LevelFilter::TRACE);
    }

    #[test]
    fn log_level_quiet_decreases() {
        assert_eq!(log_level(0, 1), LevelFilter::WARN);
        assert_eq!(log_level(0, 2), LevelFilter::ERROR);
        assert_eq!(log_level(0, 3), LevelFilter::OFF);
    }

    #[test]
    fn log_level_clamps_at_boundaries() {
        assert_eq!(log_level(10, 0), LevelFilter::TRACE);
        assert_eq!(log_level(0, 10), LevelFilter::OFF);
    }

    #[test]
    fn log_level_verbose_and_quiet_cancel() {
        assert_eq!(log_level(1, 1), LevelFilter::INFO);
        assert_eq!(log_level(2, 1), LevelFilter::DEBUG);
    }
}
