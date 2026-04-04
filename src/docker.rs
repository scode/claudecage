use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tracing::{debug, info};

use crate::mounts::Mount;

const IMAGE_NAME: &str = "claudecage:latest";
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
/// The image includes a non-root user matching the host user's uid/gid
/// so claude doesn't refuse to run (it rejects root).
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

    info!("building Docker image {IMAGE_NAME}");

    let mut cmd = Command::new("docker");
    cmd.args(build_command_args(
        mode,
        &dockerfile_path,
        tmp.path(),
        &build_context,
    ));

    let status = cmd.status().context("failed to run docker build")?;

    if !status.success() {
        bail!("docker build failed with {status}");
    }

    Ok(())
}

fn build_command_args(
    mode: BuildMode,
    dockerfile_path: &Path,
    context_path: &Path,
    build_context: &BuildContext,
) -> Vec<String> {
    let mut args = vec![
        "build".to_string(),
        "-t".to_string(),
        IMAGE_NAME.to_string(),
        "-f".to_string(),
        dockerfile_path.display().to_string(),
        "--build-arg".to_string(),
        format!("USERNAME={}", build_context.username),
        "--build-arg".to_string(),
        format!("UID={}", build_context.uid),
        "--build-arg".to_string(),
        format!("GID={}", build_context.gid),
        "--build-arg".to_string(),
        format!("HOST_HOME={}", build_context.host_home),
    ];

    if mode == BuildMode::Refresh {
        let refresh_token = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos().to_string())
            .unwrap_or_else(|_| format!("fallback-{}", std::process::id()));
        args.push("--build-arg".to_string());
        args.push(format!("REFRESH_TOKEN={refresh_token}"));
    }

    if mode == BuildMode::Rebuild {
        args.push("--no-cache".to_string());
    }

    args.push(context_path.display().to_string());
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
    Shell(&'a [String]),
    Run(&'a str),
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

    match entrypoint {
        Entrypoint::Claude(claude_args) => {
            // Suppress the interactive TOS prompt — redundant inside a sandbox.
            cmd.args([
                "claude",
                "--dangerously-skip-permissions",
                "--settings",
                r#"{"skipDangerousModePermissionPrompt": true}"#,
            ]);
            cmd.args(claude_args);
        }
        Entrypoint::Shell(shell_args) => {
            cmd.arg("bash");
            cmd.args(shell_args);
        }
        Entrypoint::Run(command) => {
            cmd.args(["bash", "-c", command]);
        }
    }

    debug!(?cmd, "docker run");

    let status = cmd.status().context("failed to run docker")?;

    Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!args.iter().any(|arg| arg.starts_with("REFRESH_TOKEN=")));
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
        assert!(args.iter().any(|arg| arg.starts_with("REFRESH_TOKEN=")));
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
        assert!(!args.iter().any(|arg| arg.starts_with("REFRESH_TOKEN=")));
    }
}
