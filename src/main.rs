mod auth;
mod docker;
mod mount_approval;
mod mounts;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use tracing::{debug, info};

#[derive(Debug)]
struct ContainerSetup {
    mounts: Vec<mounts::Mount>,
    // The canonical project path defines which rw mounts are "the project" for
    // approval purposes, even when Codex gets an extra visible-path alias mount.
    project_root: PathBuf,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum MountProfile {
    All,
    Claude,
    Codex,
    Shell,
    Run,
}

fn mount_profile_for_listing(profile: MountProfile) -> &'static [mounts::AgentStateDir] {
    match profile {
        MountProfile::All => &[mounts::AgentStateDir::Claude, mounts::AgentStateDir::Codex],
        MountProfile::Claude => &[mounts::AgentStateDir::Claude],
        MountProfile::Codex => &[mounts::AgentStateDir::Codex],
        MountProfile::Shell | MountProfile::Run => {
            &[mounts::AgentStateDir::Claude, mounts::AgentStateDir::Codex]
        }
    }
}

fn mount_profile_label(profile: MountProfile) -> &'static str {
    match profile {
        MountProfile::All => "all",
        MountProfile::Claude => "claude",
        MountProfile::Codex => "codex",
        MountProfile::Shell => "shell",
        MountProfile::Run => "run",
    }
}

fn mount_profiles_to_print(profile: MountProfile) -> &'static [MountProfile] {
    match profile {
        MountProfile::All => &[
            MountProfile::Claude,
            MountProfile::Codex,
            MountProfile::Shell,
            MountProfile::Run,
        ],
        MountProfile::Claude => &[MountProfile::Claude],
        MountProfile::Codex => &[MountProfile::Codex],
        MountProfile::Shell => &[MountProfile::Shell],
        MountProfile::Run => &[MountProfile::Run],
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
        /// Arguments joined with spaces and passed as a single string to bash -c.
        #[arg(required = true)]
        command: Vec<String>,
    },
    /// Show what mounts would be created in the container.
    Mounts {
        /// Which command profile to show mounts for. Defaults to all profiles.
        #[arg(value_enum, default_value_t = MountProfile::All)]
        profile: MountProfile,
    },
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

/// Resolve mounts for an explicit working directory.
///
/// Production code uses `current_dir()`, but tests should be able to exercise
/// the launch path without mutating process-global CWD and risking leaked state
/// if a panic interrupts cleanup.
fn resolve_mounts_for_workdir(
    home: &Path,
    workdir: &Path,
    agent_state_dirs: &[mounts::AgentStateDir],
    preserve_visible_host_workdir: bool,
    materialize_state: bool,
) -> Result<ContainerSetup> {
    let canonical_home = home
        .canonicalize()
        .context("failed to resolve home directory")?;
    let canonical_workdir = workdir
        .canonicalize()
        .context("failed to resolve working directory")?;
    if !canonical_workdir.starts_with(&canonical_home) {
        bail!(
            "working directory {} is outside the home directory — \
             only projects under $HOME are accessible in the container",
            workdir.display()
        );
    }
    let username = std::env::var("USER").context("USER environment variable not set")?;
    let container_home = PathBuf::from(format!("/home/{username}"));
    let mut mounts = if materialize_state {
        mounts::resolve_mounts(home, &container_home, &canonical_workdir, agent_state_dirs)?
    } else {
        mounts::preview_mounts(home, &container_home, &canonical_workdir, agent_state_dirs)?
    };
    debug!(count = mounts.len(), "resolved mounts");
    let host_workdir = if preserve_visible_host_workdir {
        let pwd = std::env::var_os("PWD").map(PathBuf::from);
        preferred_host_workdir(home, &canonical_workdir, pwd.as_deref())
    } else {
        canonical_workdir.clone()
    };
    if preserve_visible_host_workdir {
        if let Some(alias_mount) = codex_project_alias_mount(
            &canonical_home,
            &container_home,
            &canonical_workdir,
            &host_workdir,
        ) {
            mounts.push(alias_mount);
        }
    }
    let container_workdir =
        mounts::remap_path(&canonical_workdir, &canonical_home, &container_home);
    Ok(ContainerSetup {
        mounts,
        project_root: canonical_workdir,
        container_workdir,
        host_workdir,
    })
}

fn preferred_host_workdir(home: &Path, canonical_workdir: &Path, pwd: Option<&Path>) -> PathBuf {
    let Some(pwd) = pwd else {
        return canonical_workdir.to_path_buf();
    };
    if !pwd.is_absolute() || !pwd.starts_with(home) {
        return canonical_workdir.to_path_buf();
    }
    match pwd.canonicalize() {
        Ok(resolved) if resolved == canonical_workdir => pwd.to_path_buf(),
        _ => canonical_workdir.to_path_buf(),
    }
}

fn codex_project_alias_mount(
    canonical_home: &Path,
    container_home: &Path,
    canonical_workdir: &Path,
    host_workdir: &Path,
) -> Option<mounts::Mount> {
    if host_workdir == canonical_workdir {
        return None;
    }

    Some(mounts::Mount {
        host_path: canonical_workdir.to_path_buf(),
        container_path: mounts::remap_path(host_workdir, canonical_home, container_home),
        readonly: false,
    })
}

/// Build the candidate mount set, enforce approval, then materialize the real
/// launch setup.
///
/// The preview pass is deliberately non-mutating. If the user rejects a changed
/// mount set, claudecage should not have already created `~/.claude`, `~/.codex`,
/// or other persistent host-side state as a side effect of asking.
fn prepare_launch_setup(
    home: &Path,
    cmd: &Command,
    stdin: &mut impl std::io::BufRead,
    output: &mut impl std::io::Write,
    interactive: bool,
) -> Result<ContainerSetup> {
    let workdir = std::env::current_dir().context("could not determine working directory")?;
    prepare_launch_setup_for_workdir(home, &workdir, cmd, stdin, output, interactive)
}

/// Prepare a launch using an explicit workdir.
///
/// This exists so tests can drive the full launch-preparation path without
/// mutating process-wide cwd. The behavior is otherwise identical to the normal
/// launch setup path.
fn prepare_launch_setup_for_workdir(
    home: &Path,
    workdir: &Path,
    cmd: &Command,
    stdin: &mut impl std::io::BufRead,
    output: &mut impl std::io::Write,
    interactive: bool,
) -> Result<ContainerSetup> {
    let preview = resolve_mounts_for_workdir(
        home,
        workdir,
        mount_profile_for_command(cmd),
        matches!(cmd, Command::Codex { .. }),
        false,
    )?;
    let preview_snapshot = mount_approval::render_snapshot(&preview.mounts, &preview.project_root);
    mount_approval::enforce_mount_approval(
        home,
        approval_profile_for_command(cmd),
        &preview.mounts,
        &preview.project_root,
        interactive,
        stdin,
        output,
    )?;
    let materialized = resolve_mounts_for_workdir(
        home,
        workdir,
        mount_profile_for_command(cmd),
        matches!(cmd, Command::Codex { .. }),
        true,
    )?;
    let materialized_snapshot =
        mount_approval::render_snapshot(&materialized.mounts, &materialized.project_root);
    if materialized_snapshot != preview_snapshot {
        bail!(
            "mount set changed after approval was granted; rerun so claudecage can show and approve the updated mount diff"
        );
    }

    Ok(materialized)
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
        Command::Run { command } => docker::Entrypoint::Run(command.join(" ")),
        _ => unreachable!(),
    }
}

/// Map launch commands onto the persisted mount-approval profiles.
///
/// `shell` and `run` intentionally share a profile because they expose the same
/// non-project mount set today. Keeping them together avoids asking the user to
/// approve the same mount diff twice under two command names.
fn approval_profile_for_command(cmd: &Command) -> mount_approval::ApprovalProfile {
    match cmd {
        Command::Claude { .. } => mount_approval::ApprovalProfile::Claude,
        Command::Codex { .. } => mount_approval::ApprovalProfile::Codex,
        Command::Shell { .. } | Command::Run { .. } => mount_approval::ApprovalProfile::ShellRun,
        Command::Mounts { .. } | Command::Image { .. } | Command::Auth { .. } => {
            unreachable!("non-launch commands do not have mount-approval profiles")
        }
    }
}

fn print_mounts(
    home: &Path,
    profile: MountProfile,
    output: &mut impl std::io::Write,
) -> Result<()> {
    let workdir = std::env::current_dir().context("could not determine working directory")?;
    print_mounts_for_workdir(home, &workdir, profile, output)
}

fn print_mounts_for_workdir(
    home: &Path,
    workdir: &Path,
    profile: MountProfile,
    output: &mut impl std::io::Write,
) -> Result<()> {
    let use_color = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let profiles = mount_profiles_to_print(profile);
    for (idx, current) in profiles.iter().copied().enumerate() {
        if profile == MountProfile::All {
            if idx > 0 {
                writeln!(output).context("failed to write mount listing")?;
            }
            writeln!(output, "[{}]", mount_profile_label(current))
                .context("failed to write mount listing")?;
        }

        let mut setup = resolve_mounts_for_workdir(
            home,
            workdir,
            mount_profile_for_listing(current),
            matches!(current, MountProfile::Codex),
            true,
        )?;
        let mut mounts = std::mem::take(&mut setup.mounts);
        mounts.sort_by(|a, b| a.host_path.cmp(&b.host_path));
        for mount in &mounts {
            let mode = match (mount.readonly, use_color) {
                (true, true) => "\x1b[90m(ro)\x1b[0m",
                (true, false) => "(ro)",
                (false, true) => "\x1b[31m(rw)\x1b[0m",
                (false, false) => "(rw)",
            };
            writeln!(
                output,
                "{} {} -> {}",
                mode,
                mount.host_path.display(),
                mount.container_path.display(),
            )
            .context("failed to write mount listing")?;
        }
    }
    Ok(())
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
            if !docker::image_exists()? {
                bail!("image not found — run 'claudecage image build' first");
            }
            let stdin = std::io::stdin();
            let stderr = std::io::stderr();
            // Approval needs a readable terminal on stdin. The diff and prompt can
            // still be emitted when stderr is redirected.
            let interactive = std::io::IsTerminal::is_terminal(&stdin);
            let mut stdin = stdin.lock();
            let mut stderr = stderr.lock();
            let setup = prepare_launch_setup(&home, cmd, &mut stdin, &mut stderr, interactive)?;
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
        Command::Mounts { profile } => {
            let stdout = std::io::stdout();
            let mut stdout = stdout.lock();
            print_mounts(&home, profile, &mut stdout)?;
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
    use std::fs;
    use std::io::Cursor;
    use std::io::Write;
    use std::io::{BufRead, Read};
    use std::os::unix::fs as unix_fs;
    use tracing::level_filters::LevelFilter;

    struct MutatingApprovalInput {
        inner: Cursor<Vec<u8>>,
        mutation_done: bool,
        source_dir: PathBuf,
        target_dir: PathBuf,
    }

    impl MutatingApprovalInput {
        fn new(source_dir: PathBuf, target_dir: PathBuf) -> Self {
            Self {
                inner: Cursor::new(b"yes\n".to_vec()),
                mutation_done: false,
                source_dir,
                target_dir,
            }
        }

        fn apply_mutation(&mut self) {
            if self.mutation_done {
                return;
            }
            fs::create_dir_all(&self.target_dir).unwrap();
            unix_fs::symlink(&self.target_dir, self.source_dir.join("skills")).unwrap();
            self.mutation_done = true;
        }
    }

    impl Read for MutatingApprovalInput {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.apply_mutation();
            self.inner.read(buf)
        }
    }

    impl BufRead for MutatingApprovalInput {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            self.apply_mutation();
            self.inner.fill_buf()
        }

        fn consume(&mut self, amt: usize) {
            self.inner.consume(amt);
        }
    }

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
    fn cli_parses_run_subcommand_as_command_parts() {
        let cli = Cli::try_parse_from(["claudecage", "run", "echo", "hi"]).unwrap();

        match cli.command {
            Command::Run { command } => {
                assert_eq!(command, vec!["echo".to_string(), "hi".to_string()]);
            }
            _ => panic!("expected run subcommand"),
        }
    }

    #[test]
    fn cli_parses_mounts_profile() {
        let cli = Cli::try_parse_from(["claudecage", "mounts", "codex"]).unwrap();

        match cli.command {
            Command::Mounts { profile } => {
                assert_eq!(profile, MountProfile::Codex);
            }
            _ => panic!("expected mounts subcommand"),
        }
    }

    #[test]
    fn cli_defaults_mounts_profile_to_all() {
        let cli = Cli::try_parse_from(["claudecage", "mounts"]).unwrap();

        match cli.command {
            Command::Mounts { profile } => {
                assert_eq!(profile, MountProfile::All);
            }
            _ => panic!("expected mounts subcommand"),
        }
    }

    #[test]
    fn codex_uses_host_workdir() {
        let setup = ContainerSetup {
            mounts: Vec::new(),
            project_root: PathBuf::from("/Users/alice/git/project"),
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
            project_root: PathBuf::from("/Users/alice/git/project"),
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

    #[test]
    fn all_listing_mount_profile_includes_both_agent_state_dirs() {
        let profile = mount_profile_for_listing(MountProfile::All);

        assert_eq!(
            profile,
            [mounts::AgentStateDir::Claude, mounts::AgentStateDir::Codex]
        );
    }

    #[test]
    fn print_mounts_all_includes_each_profile_section() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = home.join("project");
        fs::create_dir_all(&project).unwrap();

        let mut output = Vec::new();
        print_mounts_for_workdir(&home, &project, MountProfile::All, &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();

        assert!(output.contains("[claude]\n"));
        assert!(output.contains("[codex]\n"));
        assert!(output.contains("[shell]\n"));
        assert!(output.contains("[run]\n"));
    }

    #[test]
    fn approval_profile_matches_command() {
        assert_eq!(
            approval_profile_for_command(&Command::Shell {
                shell_args: Vec::new(),
            }),
            mount_approval::ApprovalProfile::ShellRun
        );
    }

    #[test]
    fn preferred_host_workdir_preserves_matching_pwd() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let src = home.join("src");
        let project = src.join("project");
        fs::create_dir_all(&project).unwrap();
        unix_fs::symlink(&src, home.join("link")).unwrap();
        let pwd = home.join("link").join("project");

        assert_eq!(
            preferred_host_workdir(&home, &project.canonicalize().unwrap(), Some(&pwd)),
            pwd
        );
    }

    #[test]
    fn preferred_host_workdir_falls_back_for_mismatched_pwd() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let src = home.join("src");
        let project = src.join("project");
        fs::create_dir_all(&project).unwrap();
        let other = home.join("other");
        fs::create_dir_all(&other).unwrap();

        assert_eq!(
            preferred_host_workdir(&home, &project.canonicalize().unwrap(), Some(&other)),
            project.canonicalize().unwrap()
        );
    }

    #[test]
    fn codex_project_alias_mount_uses_visible_host_path() {
        let mount = codex_project_alias_mount(
            Path::new("/Users/alice"),
            Path::new("/home/alice"),
            Path::new("/Users/alice/src/project"),
            Path::new("/Users/alice/link/project"),
        )
        .expect("expected alias mount");

        assert_eq!(mount.host_path, Path::new("/Users/alice/src/project"));
        assert_eq!(mount.container_path, Path::new("/home/alice/link/project"));
        assert!(!mount.readonly);
    }

    #[test]
    fn prepare_launch_setup_rejects_without_creating_state() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = home.join("project");
        fs::create_dir_all(&project).unwrap();

        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();
        let err = prepare_launch_setup_for_workdir(
            &home,
            &project,
            &Command::Claude {
                claude_args: Vec::new(),
            },
            &mut input,
            &mut output,
            false,
        )
        .unwrap_err();

        assert!(err.to_string().contains("rerun interactively"));
        assert!(!home.join(".claude").exists());
        assert!(!home.join(".claudecage").exists());
    }

    #[test]
    fn prepare_launch_setup_materializes_state_after_approval() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = home.join("project");
        fs::create_dir_all(&project).unwrap();

        let mut input = Cursor::new("yes\n");
        let mut output = Vec::new();
        let setup = prepare_launch_setup_for_workdir(
            &home,
            &project,
            &Command::Claude {
                claude_args: Vec::new(),
            },
            &mut input,
            &mut output,
            true,
        )
        .unwrap();

        assert!(home.join(".claude").exists());
        assert!(home
            .join(".claudecage")
            .join("approved-mounts")
            .join("claude.txt")
            .exists());
        assert!(
            !setup.mounts.is_empty(),
            "approved launch should return a real container setup"
        );
    }

    #[test]
    fn prepare_launch_setup_preview_and_materialized_snapshots_match() {
        let tmp = tempfile::tempdir().unwrap();
        let real_home = tmp.path().join("real-home");
        let aliased_root = tmp.path().join("aliased");
        fs::create_dir_all(&real_home).unwrap();
        fs::create_dir_all(&aliased_root).unwrap();
        let project = real_home.join("project");
        fs::create_dir_all(&project).unwrap();
        let aliased_home = aliased_root.join("home-link");
        unix_fs::symlink(&real_home, &aliased_home).unwrap();

        let preview = resolve_mounts_for_workdir(
            &aliased_home,
            &project,
            &[mounts::AgentStateDir::Claude],
            false,
            false,
        )
        .unwrap();
        let materialized = resolve_mounts_for_workdir(
            &aliased_home,
            &project,
            &[mounts::AgentStateDir::Claude],
            false,
            true,
        )
        .unwrap();

        assert_eq!(
            mount_approval::render_snapshot(&preview.mounts, &preview.project_root),
            mount_approval::render_snapshot(&materialized.mounts, &materialized.project_root)
        );
    }

    #[test]
    fn prepare_launch_setup_rejects_mount_changes_after_approval() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = home.join("project");
        let claude_dir = home.join(".claude");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&claude_dir).unwrap();
        let target_dir = home.join("late-added-target");

        let mut input = MutatingApprovalInput::new(claude_dir, target_dir);
        let mut output = Vec::new();
        let err = prepare_launch_setup_for_workdir(
            &home,
            &project,
            &Command::Claude {
                claude_args: Vec::new(),
            },
            &mut input,
            &mut output,
            true,
        )
        .unwrap_err();

        assert!(err
            .to_string()
            .contains("mount set changed after approval was granted"));
    }

    #[test]
    fn print_mounts_does_not_create_approval_snapshots() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let project = home.join("project");
        fs::create_dir_all(&project).unwrap();

        let mut output = Vec::new();
        let setup = resolve_mounts_for_workdir(
            &home,
            &project,
            mount_profile_for_listing(MountProfile::Shell),
            false,
            true,
        )
        .unwrap();
        let mut mounts = setup.mounts;
        mounts.sort_by(|a, b| a.host_path.cmp(&b.host_path));
        for mount in &mounts {
            writeln!(
                output,
                "{} {} -> {}",
                if mount.readonly { "(ro)" } else { "(rw)" },
                mount.host_path.display(),
                mount.container_path.display(),
            )
            .unwrap();
        }

        assert!(!home.join(".claudecage").join("approved-mounts").exists());
        assert!(!output.is_empty());
    }
}
