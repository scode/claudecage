use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::{bail, Context, Result};
use tracing::{debug, info};

use crate::mounts::Mount;

const IMAGE_NAME: &str = "claudecage:latest";
const CONTAINER_NAME: &str = "claudecage";
const DOCKERFILE: &str = include_str!("dockerfile.txt");

pub fn image_exists() -> Result<bool> {
    let output = Command::new("docker")
        .args(["image", "inspect", IMAGE_NAME])
        .output()
        .context("failed to run docker image inspect")?;

    Ok(output.status.success())
}

/// Build the Docker image from the embedded Dockerfile.
pub fn build_image() -> Result<()> {
    let tmp = tempfile::tempdir().context("failed to create temp dir for Dockerfile")?;
    let dockerfile_path = tmp.path().join("Dockerfile");
    std::fs::write(&dockerfile_path, DOCKERFILE).context("failed to write Dockerfile")?;

    info!("building Docker image {IMAGE_NAME}");

    let status = Command::new("docker")
        .args(["build", "-t", IMAGE_NAME, "-f"])
        .arg(&dockerfile_path)
        .arg(tmp.path())
        .status()
        .context("failed to run docker build")?;

    if !status.success() {
        bail!("docker build failed with {status}");
    }

    Ok(())
}

/// Create the claudecage container with the given mounts and start it.
///
/// Removes any existing container first so init is idempotent. Creates a
/// non-root user inside the container matching the host user's uid/gid so
/// that claude doesn't refuse to run (it rejects root). Capabilities are
/// dropped to reduce the attack surface.
pub fn create_container(mounts: &[Mount], home: &Path) -> Result<()> {
    if container_exists()? {
        info!("removing existing container '{CONTAINER_NAME}'");
        let status = Command::new("docker")
            .args(["rm", "-f", CONTAINER_NAME])
            .status()
            .context("failed to remove existing container")?;
        if !status.success() {
            bail!("failed to remove existing container '{CONTAINER_NAME}'");
        }
    }

    let home_str = home
        .to_str()
        .context("home directory path is not valid UTF-8")?;

    let mut cmd = Command::new("docker");
    cmd.args([
        "create",
        "--name",
        CONTAINER_NAME,
        "--cap-drop=ALL",
        "--security-opt=no-new-privileges",
        "-e",
    ]);
    cmd.arg(format!("HOME={home_str}"));

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

        // Use --mount instead of -v to avoid ambiguity with colons in paths.
        cmd.arg("--mount");
        cmd.arg(format!(
            "type=bind,source={host},target={container}{readonly}"
        ));
    }

    cmd.args([IMAGE_NAME, "sleep", "infinity"]);

    debug!(?cmd, "docker create");

    let status = cmd.status().context("failed to run docker create")?;

    if !status.success() {
        bail!("docker create failed with {status}");
    }

    start_container()?;
    setup_container_user(home_str)?;

    info!("container '{CONTAINER_NAME}' created and started");
    Ok(())
}

/// Create a user inside the container matching the host user.
///
/// Claude refuses to run as root, so we need a non-root user with the
/// same uid/gid as the host user. This runs as root (the container's
/// default) to create the user, then all subsequent claude invocations
/// use --user.
fn setup_container_user(home: &str) -> Result<()> {
    let uid = nix::unistd::getuid().as_raw();
    let gid = nix::unistd::getgid().as_raw();
    let username = std::env::var("USER").context("USER environment variable not set")?;

    info!("creating container user {username} (uid={uid}, gid={gid})");

    let setup_script = format!(
        "groupadd -g {gid} {username} 2>/dev/null; \
         useradd -u {uid} -g {gid} -d {home} -s /bin/bash {username}",
    );

    let status = Command::new("docker")
        .args([
            "exec",
            CONTAINER_NAME,
            "bash",
            "-c",
            &setup_script,
        ])
        .status()
        .context("failed to create container user")?;

    if !status.success() {
        bail!("failed to create user '{username}' in container");
    }

    Ok(())
}

/// The --user flag for docker exec, matching the host user.
fn user_flag() -> Result<String> {
    let uid = nix::unistd::getuid().as_raw();
    let gid = nix::unistd::getgid().as_raw();
    Ok(format!("{uid}:{gid}"))
}

/// Execute claude inside the container with the given working directory and args.
pub fn exec_claude(workdir: &Path, claude_args: &[String]) -> Result<ExitCode> {
    ensure_running()?;

    let workdir_str = workdir
        .to_str()
        .context("working directory is not valid UTF-8")?;

    let user = user_flag()?;
    let mut args = vec![
        "exec",
        "-it",
        "--user",
        &user,
        "-w",
        workdir_str,
        CONTAINER_NAME,
        "claude",
        "--dangerously-skip-permissions",
    ];

    args.extend(claude_args.iter().map(String::as_str));

    debug!(?args, "docker exec");

    let status = Command::new("docker")
        .args(&args)
        .status()
        .context("failed to run docker exec")?;

    Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
}

/// Update packages and claude inside the running container.
/// Runs as root since apt/npm require it.
pub fn exec_refresh() -> Result<()> {
    ensure_running()?;

    info!("refreshing container packages");

    let status = Command::new("docker")
        .args([
            "exec",
            "-e",
            "DEBIAN_FRONTEND=noninteractive",
            CONTAINER_NAME,
            "bash",
            "-c",
            "apt-get update && apt-get upgrade -y && npm update -g @anthropic-ai/claude-code",
        ])
        .status()
        .context("failed to run refresh")?;

    if !status.success() {
        bail!("container refresh failed with {status}");
    }

    Ok(())
}

/// Run the claude auth flow interactively inside the container.
pub fn exec_auth() -> Result<()> {
    ensure_running()?;

    let user = user_flag()?;
    let status = Command::new("docker")
        .args(["exec", "-it", "--user", &user, CONTAINER_NAME, "claude", "login"])
        .status()
        .context("failed to run claude login")?;

    if !status.success() {
        bail!("claude login failed with {status}");
    }

    Ok(())
}

fn container_exists() -> Result<bool> {
    let output = Command::new("docker")
        .args(["inspect", CONTAINER_NAME])
        .output()
        .context("failed to run docker inspect")?;

    Ok(output.status.success())
}

fn container_running() -> Result<bool> {
    let output = Command::new("docker")
        .args([
            "inspect",
            "-f",
            "{{.State.Running}}",
            CONTAINER_NAME,
        ])
        .output()
        .context("failed to run docker inspect")?;

    Ok(output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true")
}

fn start_container() -> Result<()> {
    let status = Command::new("docker")
        .args(["start", CONTAINER_NAME])
        .status()
        .context("failed to start container")?;

    if !status.success() {
        bail!("docker start failed with {status}");
    }

    Ok(())
}

/// Ensure the container is running. If the container exists but is stopped
/// (e.g., after a Docker daemon restart), start it automatically so the
/// user doesn't have to care about container state.
fn ensure_running() -> Result<()> {
    if !container_exists()? {
        bail!("container '{CONTAINER_NAME}' does not exist — run 'claudecage container init' first");
    }
    if !container_running()? {
        info!("container '{CONTAINER_NAME}' is stopped, starting it");
        start_container()?;
    }
    Ok(())
}
