use std::ffi::OsString;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode, ExitStatus};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tracing::{debug, info};

use crate::mounts::Mount;

const IMAGE_NAME: &str = "claudecage:latest";
const IMAGE_LABEL_KEY: &str = "org.scode.claudecage";
const IMAGE_LABEL_VALUE: &str = "true";
const DOCKERFILE: &str = include_str!("dockerfile.txt");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BuildMode {
    Build,
    Refresh,
    Rebuild,
}

#[derive(Debug, Eq, PartialEq)]
struct BuildContext {
    username: String,
    uid: String,
    gid: String,
    host_home: String,
}

pub fn image_exists() -> Result<bool> {
    let output = Command::new("docker")
        .args(["image", "inspect", IMAGE_NAME])
        .output()
        .context("failed to run docker image inspect")?;

    if output.status.success() {
        return Ok(true);
    }

    // "No such image" means the image genuinely doesn't exist.
    // Any other failure (e.g., daemon not running) is surfaced as an error
    // with docker's own stderr so the user sees the actual problem.
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("No such image") {
        Ok(false)
    } else {
        bail!("{}", stderr.trim());
    }
}

/// Build the Docker image from the embedded Dockerfile.
///
/// The image includes a non-root user matching the host user's uid/gid so the
/// bundled agents do not run as root inside the container.
pub fn build_image(mode: BuildMode) -> Result<()> {
    let tmp = tempfile::tempdir().context("failed to create temp dir for Dockerfile")?;
    let dockerfile_path = tmp.path().join("Dockerfile");
    std::fs::write(&dockerfile_path, DOCKERFILE).context("failed to write Dockerfile")?;

    let build_context = BuildContext {
        uid: nix::unistd::getuid().as_raw().to_string(),
        gid: nix::unistd::getgid().as_raw().to_string(),
        username: std::env::var("USER").context("USER environment variable not set")?,
        host_home: dirs::home_dir()
            .context("could not determine home directory")?
            .to_str()
            .context("home directory is not valid UTF-8")?
            .to_string(),
    };

    run_image_build(mode, &dockerfile_path, tmp.path(), &build_context, |args| {
        let mut cmd = Command::new("docker");
        cmd.args(args);
        cmd.status().context("failed to run docker")
    })
}

fn run_image_build(
    mode: BuildMode,
    dockerfile_path: &Path,
    context_path: &Path,
    build_context: &BuildContext,
    mut run_docker: impl FnMut(&[OsString]) -> Result<ExitStatus>,
) -> Result<()> {
    run_prune(&mut run_docker)?;

    info!("building Docker image {IMAGE_NAME}");
    let build_args = build_command_args(mode, dockerfile_path, context_path, build_context);
    let status = run_docker(&build_args).context("failed to run docker build")?;
    let post_build_prune = run_prune(&mut run_docker);

    if !status.success() {
        if let Err(err) = post_build_prune {
            debug!(
                ?err,
                "post-build claudecage image prune failed after unsuccessful build"
            );
        }
        bail!("docker build failed with {status}");
    }

    post_build_prune?;

    Ok(())
}

/// Remove old claudecage image objects without touching other Docker state.
///
/// Docker keeps the previous image object around as dangling data when a build
/// retags `claudecage:latest`. Those old objects are ours, but the global
/// builder cache is shared with every other Docker workload on the machine, so
/// this deliberately does not run `docker system prune` or `docker builder
/// prune`. Running this before and after a build keeps both stale leftovers and
/// the just-replaced image from accumulating.
fn run_prune(run_docker: &mut impl FnMut(&[OsString]) -> Result<ExitStatus>) -> Result<()> {
    info!("pruning old claudecage Docker images");
    let status =
        run_docker(&prune_claudecage_image_args()).context("failed to run docker image prune")?;

    if !status.success() {
        bail!("docker image prune failed with {status}");
    }

    Ok(())
}

fn image_label() -> String {
    format!("{IMAGE_LABEL_KEY}={IMAGE_LABEL_VALUE}")
}

fn image_label_filter() -> String {
    format!("label={}", image_label())
}

fn prune_claudecage_image_args() -> Vec<OsString> {
    vec![
        OsString::from("image"),
        OsString::from("prune"),
        OsString::from("--force"),
        OsString::from("--filter"),
        OsString::from(image_label_filter()),
    ]
}

fn build_command_args(
    mode: BuildMode,
    dockerfile_path: &Path,
    context_path: &Path,
    build_context: &BuildContext,
) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("build"),
        OsString::from("-t"),
        OsString::from(IMAGE_NAME),
        OsString::from("--label"),
        OsString::from(image_label()),
        OsString::from("-f"),
        dockerfile_path.as_os_str().to_os_string(),
        OsString::from("--build-arg"),
        OsString::from(format!("USERNAME={}", build_context.username)),
        OsString::from("--build-arg"),
        OsString::from(format!("UID={}", build_context.uid)),
        OsString::from("--build-arg"),
        OsString::from(format!("GID={}", build_context.gid)),
        OsString::from("--build-arg"),
        OsString::from(format!("HOST_HOME={}", build_context.host_home)),
    ];

    if mode == BuildMode::Refresh {
        // Refresh works by changing a build arg that is only in scope for the
        // Dockerfile's tail layers, so cached base layers remain reusable.
        let refresh_marker = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos().to_string())
            .unwrap_or_else(|_| format!("fallback-{}", std::process::id()));
        args.push(OsString::from("--build-arg"));
        args.push(OsString::from(format!("REFRESH_MARKER={refresh_marker}")));
    }

    if mode == BuildMode::Rebuild {
        args.push(OsString::from("--no-cache"));
        args.push(OsString::from("--pull"));
    }

    args.push(context_path.as_os_str().to_os_string());
    args
}

/// Write env vars in docker `--env-file` format (`KEY=VALUE\n` per entry).
fn write_env_file(writer: &mut impl Write, env_vars: &[(&str, &str)]) -> Result<()> {
    for (key, value) in env_vars {
        writeln!(writer, "{key}={value}").context("failed to write to env pipe")?;
    }
    Ok(())
}

pub enum Entrypoint<'a> {
    Claude(&'a [String]),
    Codex(&'a [String]),
    Shell(&'a [String]),
    Run(String),
}

fn entrypoint_args(entrypoint: Entrypoint<'_>) -> Vec<OsString> {
    let mut args = Vec::new();
    match entrypoint {
        Entrypoint::Claude(claude_args) => {
            args.extend([
                OsString::from("claude"),
                OsString::from("--dangerously-skip-permissions"),
                OsString::from("--settings"),
                OsString::from(r#"{"skipDangerousModePermissionPrompt": true}"#),
            ]);
            args.extend(claude_args.iter().cloned().map(OsString::from));
        }
        Entrypoint::Codex(codex_args) => {
            args.extend([
                OsString::from("codex"),
                OsString::from("--dangerously-bypass-approvals-and-sandbox"),
                OsString::from("-c"),
                OsString::from(r#"cli_auth_credentials_store="file""#),
            ]);
            args.extend(codex_args.iter().cloned().map(OsString::from));
        }
        Entrypoint::Shell(shell_args) => {
            args.push(OsString::from("bash"));
            args.extend(shell_args.iter().cloned().map(OsString::from));
        }
        Entrypoint::Run(command) => {
            args.extend([
                OsString::from("bash"),
                OsString::from("-c"),
                OsString::from(command),
            ]);
        }
    }
    args
}

/// Run an ephemeral container with the given mounts and working directory.
///
/// Values in `env_vars` do not appear in process argument lists on the host.
pub fn run_container(
    mounts: &[Mount],
    workdir: &Path,
    entrypoint: Entrypoint<'_>,
    env_vars: &[(&str, &str)],
) -> Result<ExitCode> {
    let workdir_str = workdir
        .to_str()
        .context("working directory is not valid UTF-8")?;

    let mut cmd = Command::new("docker");
    cmd.args(["run", "--rm"]);
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        cmd.arg("-it");
    }
    cmd.args(["--cap-drop=ALL", "--security-opt=no-new-privileges"]);

    for mount in mounts {
        let host = mount
            .host_path
            .to_str()
            .context("mount path is not valid UTF-8")?;
        let container = mount
            .container_path
            .to_str()
            .context("mount path is not valid UTF-8")?;
        let readonly = if mount.readonly { ",readonly" } else { "" };

        cmd.arg("--mount");
        cmd.arg(format!(
            "type=bind,source={host},target={container}{readonly}"
        ));
    }

    cmd.args(["-w", workdir_str]);

    // Kept alive so the pipe read fd remains valid through cmd.status().
    let _pipe_read = if !env_vars.is_empty() {
        let (pipe_read, pipe_write) = nix::unistd::pipe().context("failed to create pipe")?;
        let mut writer: std::fs::File = pipe_write.into();
        write_env_file(&mut writer, env_vars)?;
        drop(writer);

        let read_fd = pipe_read.as_raw_fd();
        // SAFETY: pre_exec runs between fork and exec in the child.
        // dup2 and close are async-signal-safe, satisfying pre_exec's contract.
        unsafe {
            cmd.pre_exec(move || {
                if read_fd != 3 {
                    nix::unistd::dup2(read_fd, 3)?;
                    nix::unistd::close(read_fd)?;
                }
                Ok(())
            });
        }
        cmd.arg("--env-file=/dev/fd/3");

        Some(pipe_read)
    } else {
        None
    };

    cmd.arg(IMAGE_NAME);

    cmd.args(entrypoint_args(entrypoint));

    debug!(?cmd, "docker run");

    let status = cmd.status().context("failed to run docker")?;

    Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn success_status() -> ExitStatus {
        ExitStatus::from_raw(0)
    }

    fn failure_status() -> ExitStatus {
        ExitStatus::from_raw(1 << 8)
    }

    fn render_args(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn build_context() -> BuildContext {
        BuildContext {
            username: "alice".to_string(),
            uid: "1000".to_string(),
            gid: "1000".to_string(),
            host_home: "/Users/alice".to_string(),
        }
    }

    #[test]
    fn write_env_file_single_var() {
        let mut buf = Vec::new();
        write_env_file(&mut buf, &[("GH_TOKEN", "ghp_abc")]).unwrap();
        assert_eq!(buf, b"GH_TOKEN=ghp_abc\n");
    }

    #[test]
    fn write_env_file_multiple_vars() {
        let mut buf = Vec::new();
        write_env_file(&mut buf, &[("A", "1"), ("B", "2")]).unwrap();
        assert_eq!(buf, b"A=1\nB=2\n");
    }

    #[test]
    fn write_env_file_empty() {
        let mut buf = Vec::new();
        write_env_file(&mut buf, &[]).unwrap();
        assert!(buf.is_empty());
    }

    #[test]
    fn build_mode_uses_cached_layers_without_refresh_arg() {
        let args = build_command_args(
            BuildMode::Build,
            Path::new("/tmp/Dockerfile"),
            Path::new("/tmp/context"),
            &build_context(),
        );

        assert!(!args.iter().any(|arg| arg == "--no-cache"));
        assert!(!args
            .iter()
            .any(|arg| arg.to_string_lossy().starts_with("REFRESH_MARKER=")));
    }

    #[test]
    fn build_labels_image_as_claudecage_owned() {
        let args = build_command_args(
            BuildMode::Build,
            Path::new("/tmp/Dockerfile"),
            Path::new("/tmp/context"),
            &build_context(),
        );

        assert!(args.windows(2).any(|window| window
            == [
                OsString::from("--label"),
                OsString::from("org.scode.claudecage=true")
            ]));
    }

    #[test]
    fn cleanup_prunes_only_dangling_claudecage_labeled_images() {
        let args = prune_claudecage_image_args();

        assert_eq!(
            render_args(&args),
            [
                "image",
                "prune",
                "--force",
                "--filter",
                "label=org.scode.claudecage=true"
            ]
        );
        assert!(!render_args(&args).contains(&"--all".to_string()));
    }

    #[test]
    fn image_build_prunes_before_and_after_successful_build() {
        let mut commands = Vec::new();

        run_image_build(
            BuildMode::Build,
            Path::new("/tmp/Dockerfile"),
            Path::new("/tmp/context"),
            &build_context(),
            |args| {
                commands.push(render_args(args));
                Ok(success_status())
            },
        )
        .unwrap();

        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0], render_args(&prune_claudecage_image_args()));
        assert_eq!(commands[1][0], "build");
        assert_eq!(commands[2], render_args(&prune_claudecage_image_args()));
    }

    #[test]
    fn image_build_prunes_after_failed_build() {
        let mut commands = Vec::new();
        let mut calls = 0;

        let err = run_image_build(
            BuildMode::Build,
            Path::new("/tmp/Dockerfile"),
            Path::new("/tmp/context"),
            &build_context(),
            |args| {
                calls += 1;
                commands.push(render_args(args));
                if calls == 2 {
                    Ok(failure_status())
                } else {
                    Ok(success_status())
                }
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("docker build failed"));
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0], render_args(&prune_claudecage_image_args()));
        assert_eq!(commands[1][0], "build");
        assert_eq!(commands[2], render_args(&prune_claudecage_image_args()));
    }

    #[test]
    fn dockerfile_cleans_package_manager_caches() {
        assert_eq!(DOCKERFILE.matches("brew cleanup --prune=all").count(), 4);
        assert_eq!(DOCKERFILE.matches(r#"rm -rf "$(brew --cache)""#).count(), 4);
        assert!(DOCKERFILE.contains("uv cache clean"));
    }

    #[test]
    fn refresh_mode_adds_refresh_arg_without_no_cache() {
        let args = build_command_args(
            BuildMode::Refresh,
            Path::new("/tmp/Dockerfile"),
            Path::new("/tmp/context"),
            &build_context(),
        );

        assert!(!args.iter().any(|arg| arg == "--no-cache"));
        assert!(args
            .iter()
            .any(|arg| arg.to_string_lossy().starts_with("REFRESH_MARKER=")));
    }

    #[test]
    fn rebuild_mode_uses_no_cache_without_refresh_arg() {
        let args = build_command_args(
            BuildMode::Rebuild,
            Path::new("/tmp/Dockerfile"),
            Path::new("/tmp/context"),
            &build_context(),
        );

        assert!(args.iter().any(|arg| arg == "--no-cache"));
        assert!(args.iter().any(|arg| arg == "--pull"));
        assert!(!args
            .iter()
            .any(|arg| arg.to_string_lossy().starts_with("REFRESH_MARKER=")));
    }

    #[test]
    fn codex_entrypoint_uses_bypass_flag_and_file_backed_auth() {
        let args = entrypoint_args(Entrypoint::Codex(&["-p".to_string(), "ping".to_string()]));
        let rendered: Vec<_> = args.iter().map(|arg| arg.to_string_lossy()).collect();
        assert_eq!(rendered[0], "codex");
        assert!(rendered.contains(&std::borrow::Cow::Borrowed(
            "--dangerously-bypass-approvals-and-sandbox"
        )));
        assert!(rendered.contains(&std::borrow::Cow::Borrowed("-c")));
        assert!(rendered.contains(&std::borrow::Cow::Borrowed(
            r#"cli_auth_credentials_store="file""#
        )));
    }

    #[test]
    fn codex_entrypoint_preserves_user_args() {
        let args = entrypoint_args(Entrypoint::Codex(&[
            "exec".to_string(),
            "--help".to_string(),
        ]));
        let rendered: Vec<_> = args.iter().map(|arg| arg.to_string_lossy()).collect();
        assert!(rendered.ends_with(&[
            std::borrow::Cow::Borrowed("exec"),
            std::borrow::Cow::Borrowed("--help"),
        ]));
    }
}
