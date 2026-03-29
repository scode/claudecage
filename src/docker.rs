use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::{bail, Context, Result};
use tracing::{debug, info};

use crate::mounts::Mount;

const IMAGE_NAME: &str = "claudecage:latest";
const DOCKERFILE: &str = include_str!("dockerfile.txt");

pub fn image_exists() -> Result<bool> {
    let output = Command::new("docker")
        .args(["image", "inspect", IMAGE_NAME])
        .output()
        .context("failed to run docker image inspect")?;

    Ok(output.status.success())
}

/// Build the Docker image from the embedded Dockerfile.
///
/// The image includes a non-root user matching the host user's uid/gid
/// so claude doesn't refuse to run (it rejects root).
pub fn build_image(no_cache: bool) -> Result<()> {
    let tmp = tempfile::tempdir().context("failed to create temp dir for Dockerfile")?;
    let dockerfile_path = tmp.path().join("Dockerfile");
    std::fs::write(&dockerfile_path, DOCKERFILE).context("failed to write Dockerfile")?;

    let uid = nix::unistd::getuid().as_raw().to_string();
    let gid = nix::unistd::getgid().as_raw().to_string();
    let username = std::env::var("USER").context("USER environment variable not set")?;

    info!("building Docker image {IMAGE_NAME}");

    let mut cmd = Command::new("docker");
    cmd.args(["build", "-t", IMAGE_NAME, "-f"]);
    cmd.arg(&dockerfile_path);
    cmd.args([
        "--build-arg", &format!("USERNAME={username}"),
        "--build-arg", &format!("UID={uid}"),
        "--build-arg", &format!("GID={gid}"),
    ]);
    if no_cache {
        cmd.arg("--no-cache");
    }
    cmd.arg(tmp.path());

    let status = cmd.status().context("failed to run docker build")?;

    if !status.success() {
        bail!("docker build failed with {status}");
    }

    Ok(())
}

/// Run claude in an ephemeral container with the given mounts and working directory.
pub fn run_claude(
    mounts: &[Mount],
    workdir: &Path,
    claude_args: &[String],
) -> Result<ExitCode> {
    let workdir_str = workdir
        .to_str()
        .context("working directory is not valid UTF-8")?;

    let mut cmd = Command::new("docker");
    cmd.args([
        "run",
        "--rm",
        "-it",
        "--cap-drop=ALL",
        "--security-opt=no-new-privileges",
    ]);

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
    cmd.arg(IMAGE_NAME);
    cmd.args(["claude", "--dangerously-skip-permissions"]);
    cmd.args(claude_args);

    debug!(?cmd, "docker run");

    let status = cmd.status().context("failed to run docker")?;

    Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
}
