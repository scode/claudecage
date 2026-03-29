use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::{bail, Context, Result};
use tracing::{debug, info};

use crate::mounts::Mount;

const IMAGE_NAME: &str = "claudecage:latest";
const CONTAINER_NAME: &str = "claudecage";
const DOCKERFILE: &str = include_str!("dockerfile.txt");

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

/// Create and start the claudecage container with the given mounts.
///
/// The container runs `sleep infinity` as its main process so it stays
/// alive for `docker exec` invocations. Capabilities are dropped to
/// reduce the attack surface.
pub fn create_container(mounts: &[Mount], home: &Path) -> Result<()> {
    if container_exists()? {
        bail!("container '{CONTAINER_NAME}' already exists — remove it first or use 'refresh'");
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

    let status = Command::new("docker")
        .args(["start", CONTAINER_NAME])
        .status()
        .context("failed to start container")?;

    if !status.success() {
        bail!("docker start failed with {status}");
    }

    info!("container '{CONTAINER_NAME}' created and started");
    Ok(())
}

/// Execute claude inside the container with the given working directory and args.
pub fn exec_claude(workdir: &Path, claude_args: &[String]) -> Result<ExitCode> {
    ensure_running()?;

    let workdir_str = workdir
        .to_str()
        .context("working directory is not valid UTF-8")?;

    let mut args = vec![
        "exec",
        "-it",
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
pub fn exec_refresh() -> Result<()> {
    ensure_running()?;

    info!("refreshing container packages");

    let status = Command::new("docker")
        .args([
            "exec",
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

    let status = Command::new("docker")
        .args(["exec", "-it", CONTAINER_NAME, "claude", "login"])
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

fn ensure_running() -> Result<()> {
    if !container_running()? {
        bail!(
            "container '{CONTAINER_NAME}' is not running — run 'claudecage container init' first"
        );
    }
    Ok(())
}
