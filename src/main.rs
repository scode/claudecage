mod auth;
mod docker;
mod mounts;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use tracing::{debug, info};

struct ContainerSetup {
    mounts: Vec<mounts::Mount>,
    container_workdir: PathBuf,
    host_workdir: PathBuf,
}

fn mount_profile_for_command(cmd: &Command) -> &'static [mounts::AgentStateDir] {
    match cmd {
        Command::Claude { .. } => &[mounts::AgentStateDir::Claude],
        Command::Codex { .. } => &[mounts::AgentStateDir::Codex],
        Command::Shell { .. } | Command::Run { .. } => {
            &[mounts::AgentStateDir::Claude, mounts::AgentStateDir::Codex]
        }
        _ => &[],
    }
}

#[derive(Parser)]
#[command(name = "claudecage")]
/// Run coding agents with full permissions inside a sandboxed Docker container.
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Increase log verbosity (repeat for more: -v debug, -vv trace).
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// Decrease log verbosity (repeat for less: -q warn, -qq error, -qqq off).
    #[arg(short = 'q', long = "quiet", action = clap::ArgAction::Count, global = true)]
    quiet: u8,
}

#[derive(Subcommand)]
enum Command {
    /// Run claude in the current working directory.
    Claude {
        /// Arguments forwarded to claude (after --).
        #[arg(last = true)]
        claude_args: Vec<String>,
    },
    /// Run codex in the current working directory.
    Codex {
        /// Arguments forwarded to codex (after --).
        #[arg(last = true)]
        codex_args: Vec<String>,
    },
    /// Open an interactive shell in the container.
    Shell {
        /// Arguments forwarded to bash (after --).
        #[arg(last = true)]
        shell_args: Vec<String>,
    },
    /// Run a command in the container via bash -c.
    Run {
        /// The command string to pass to bash -c.
        command: String,
    },
    /// Show what mounts would be created in the container.
    Mounts,
    /// Manage the claudecage Docker image.
    Image {
        #[command(subcommand)]
        action: ImageAction,
    },
    /// Manage credentials.
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
}

#[derive(Subcommand)]
enum ImageAction {
    /// Build the Docker image.
    Build,
    /// Refresh Claude Code, Codex CLI, and stax while preserving cached base layers.
    Refresh,
    /// Rebuild the image from scratch (no cache).
    Rebuild,
}

#[derive(Subcommand)]
enum AuthAction {
    /// Store a GitHub token in the macOS Keychain.
    SetGithubToken,
    /// Remove the stored GitHub token from the macOS Keychain.
    RemoveGithubToken,
}

/// Read a line from stdin. When stdin is a terminal, echo is disabled to avoid
/// leaking secrets into scrollback. When piped, reads plainly.
fn read_secret_line() -> Result<String> {
    let stdin = std::io::stdin();
    let is_tty = std::io::IsTerminal::is_terminal(&stdin);

    let orig = if is_tty {
        let orig =
            nix::sys::termios::tcgetattr(&stdin).context("failed to get terminal attributes")?;
        let mut noecho = orig.clone();
        noecho
            .local_flags
            .remove(nix::sys::termios::LocalFlags::ECHO);
        nix::sys::termios::tcsetattr(&stdin, nix::sys::termios::SetArg::TCSANOW, &noecho)
            .context("failed to disable terminal echo")?;
        Some(orig)
    } else {
        None
    };

    let mut line = String::new();
    let result = stdin.read_line(&mut line);

    if let Some(orig) = orig {
        let _ = nix::sys::termios::tcsetattr(&stdin, nix::sys::termios::SetArg::TCSANOW, &orig);
        eprintln!(); // Raw newline to compensate for suppressed echo, not logging.
    }

    result.context("failed to read from stdin")?;
    Ok(line.trim().to_string())
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

/// Validate the working directory and resolve mounts and both visible workdir paths.
fn resolve_mounts(
    home: &Path,
    agent_state_dirs: &[mounts::AgentStateDir],
) -> Result<ContainerSetup> {
    let workdir = std::env::current_dir().context("could not determine working directory")?;
    if !workdir.starts_with(home) {
        bail!(
            "working directory {} is outside the home directory — \
             only projects under $HOME are accessible in the container",
            workdir.display()
        );
    }
    let username = std::env::var("USER").context("USER environment variable not set")?;
    let container_home = PathBuf::from(format!("/home/{username}"));
    let mounts = mounts::resolve_mounts(home, &container_home, &workdir, agent_state_dirs)?;
    debug!(count = mounts.len(), "resolved mounts");
    let host_workdir = workdir
        .canonicalize()
        .context("failed to resolve working directory")?;
    let container_workdir = mounts::remap_path(
        &host_workdir,
        &home
            .canonicalize()
            .context("failed to resolve home directory")?,
        &container_home,
    );
    Ok(ContainerSetup {
        mounts,
        container_workdir,
        host_workdir,
    })
}

/// Resolve mounts and verify the docker image exists.
fn resolve_container_setup(
    home: &Path,
    agent_state_dirs: &[mounts::AgentStateDir],
) -> Result<ContainerSetup> {
    if !docker::image_exists()? {
        bail!("image not found — run 'claudecage image build' first");
    }
    resolve_mounts(home, agent_state_dirs)
}

fn run_image_action(action: ImageAction) -> Result<()> {
    match action {
        ImageAction::Rebuild => {
            docker::build_image(docker::BuildMode::Rebuild)?;
        }
        ImageAction::Refresh => {
            docker::build_image(docker::BuildMode::Refresh)?;
        }
        ImageAction::Build => {
            if docker::image_exists()? {
                info!(
                    "image already exists (use 'claudecage image refresh' to refresh Claude/Codex/stax or 'claudecage image rebuild' to rebuild from scratch)"
                );
            } else {
                docker::build_image(docker::BuildMode::Build)?;
            }
        }
    }

    Ok(())
}

fn workdir_for_command<'a>(cmd: &Command, setup: &'a ContainerSetup) -> &'a Path {
    match cmd {
        Command::Codex { .. } => &setup.host_workdir,
        Command::Claude { .. } | Command::Shell { .. } | Command::Run { .. } => {
            &setup.container_workdir
        }
        _ => unreachable!(),
    }
}

fn entrypoint_for_command<'a>(cmd: &'a Command) -> docker::Entrypoint<'a> {
    match cmd {
        Command::Claude { claude_args } => docker::Entrypoint::Claude(claude_args),
        Command::Codex { codex_args } => docker::Entrypoint::Codex(codex_args),
        Command::Shell { shell_args } => docker::Entrypoint::Shell(shell_args),
        Command::Run { command } => docker::Entrypoint::Run(command),
        _ => unreachable!(),
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_max_level(log_level(cli.verbose, cli.quiet))
        .init();

    let home = dirs::home_dir().context("could not determine home directory")?;

    match cli.command {
        Command::Image { action } => {
            run_image_action(action)?;
            Ok(ExitCode::SUCCESS)
        }
        ref cmd @ (Command::Claude { .. }
        | Command::Codex { .. }
        | Command::Shell { .. }
        | Command::Run { .. }) => {
            let setup = resolve_container_setup(&home, mount_profile_for_command(cmd))?;
            let github_token = auth::resolve_github_token()?;
            let env_vars: Vec<(&str, &str)> = github_token
                .as_deref()
                .map(|t| vec![("GH_TOKEN", t)])
                .unwrap_or_default();
            docker::run_container(
                &setup.mounts,
                workdir_for_command(cmd, &setup),
                entrypoint_for_command(cmd),
                &env_vars,
            )
        }
        Command::Auth { action } => {
            match action {
                AuthAction::SetGithubToken => {
                    info!("Paste a GitHub personal access token:");
                    let token = read_secret_line().context("failed to read token from stdin")?;
                    auth::validate_github_token(&token)?;
                    auth::store_github_token(&token)?;
                    info!("token stored in macOS Keychain");
                }
                AuthAction::RemoveGithubToken => {
                    auth::remove_github_token()?;
                    info!("token removed from macOS Keychain");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Mounts => {
            let mut mounts = resolve_mounts(
                &home,
                &[mounts::AgentStateDir::Claude, mounts::AgentStateDir::Codex],
            )?
            .mounts;
            mounts.sort_by(|a, b| a.host_path.cmp(&b.host_path));
            let use_color = std::io::IsTerminal::is_terminal(&std::io::stdout());
            for mount in &mounts {
                let mode = match (mount.readonly, use_color) {
                    (true, true) => "\x1b[90m(ro)\x1b[0m",
                    (true, false) => "(ro)",
                    (false, true) => "\x1b[31m(rw)\x1b[0m",
                    (false, false) => "(rw)",
                };
                println!(
                    "{} {} -> {}",
                    mode,
                    mount.host_path.display(),
                    mount.container_path.display(),
                );
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            // eprintln, not tracing — this runs before/outside the tracing subscriber.
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
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

    #[test]
    fn cli_parses_codex_subcommand() {
        let cli = Cli::try_parse_from(["claudecage", "codex", "--", "-p", "ping"]).unwrap();

        match cli.command {
            Command::Codex { codex_args } => {
                assert_eq!(codex_args, vec!["-p".to_string(), "ping".to_string()]);
            }
            _ => panic!("expected codex subcommand"),
        }
    }

    #[test]
    fn codex_uses_host_workdir() {
        let setup = ContainerSetup {
            mounts: Vec::new(),
            container_workdir: PathBuf::from("/home/alice/git/project"),
            host_workdir: PathBuf::from("/Users/alice/git/project"),
        };
        let cmd = Command::Codex {
            codex_args: Vec::new(),
        };

        assert_eq!(
            workdir_for_command(&cmd, &setup),
            Path::new("/Users/alice/git/project")
        );
    }

    #[test]
    fn claude_uses_container_workdir() {
        let setup = ContainerSetup {
            mounts: Vec::new(),
            container_workdir: PathBuf::from("/home/alice/git/project"),
            host_workdir: PathBuf::from("/Users/alice/git/project"),
        };
        let cmd = Command::Claude {
            claude_args: Vec::new(),
        };

        assert_eq!(
            workdir_for_command(&cmd, &setup),
            Path::new("/home/alice/git/project")
        );
    }

    #[test]
    fn claude_mount_profile_excludes_codex_state() {
        let profile = mount_profile_for_command(&Command::Claude {
            claude_args: Vec::new(),
        });

        assert_eq!(profile, [mounts::AgentStateDir::Claude]);
    }

    #[test]
    fn shell_mount_profile_includes_both_agent_state_dirs() {
        let profile = mount_profile_for_command(&Command::Shell {
            shell_args: Vec::new(),
        });

        assert_eq!(
            profile,
            [mounts::AgentStateDir::Claude, mounts::AgentStateDir::Codex]
        );
    }
}
