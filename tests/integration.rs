//! Integration tests gated by `CLAUDECAGE_TEST_CAPABILITIES`.
//!
//! # Capabilities
//!
//! - `docker` — Docker daemon is available. Tests that need a pre-built claudecage
//!   image assume one already exists (e.g., from a prior `claudecage image build`).
//!   Use this when iterating locally to skip the slow image build.
//!
//! - `docker_build` — Implies `docker`. Enables the image build test, which runs
//!   `claudecage image rebuild` (a full no-cache build). Other tests that need the
//!   image will also trigger a build via `ensure_image()` when this capability is set.
//!   Use this in CI or when you need to verify the Dockerfile itself. Tests may also
//!   exercise `claudecage image refresh`, which should succeed against the same image
//!   tag used by normal runs.
//!
//! - `claude_auth` — Claude is authenticated inside the container (requires prior
//!   `/login`). The image must already exist or `docker_build` must also be set.
//!
//! # Examples
//!
//! ```sh
//! # Fast local iteration: skip image build, assume image exists
//! CLAUDECAGE_TEST_CAPABILITIES=docker cargo test
//!
//! # Full CI run: build image from scratch, then run docker tests
//! CLAUDECAGE_TEST_CAPABILITIES=docker,docker_build cargo test
//!
//! # Everything including end-to-end claude test
//! CLAUDECAGE_TEST_CAPABILITIES=docker,docker_build,claude_auth cargo test
//! ```

mod common;

use std::process::Command;
use std::sync::Mutex;
use std::sync::Once;

// All integration tests live in one binary. `BUILD_IMAGE` avoids redundant
// full rebuilds, while `IMAGE_TEST_LOCK` serializes commands that mutate the
// shared `claudecage:latest` tag.
static BUILD_IMAGE: Once = Once::new();
static IMAGE_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Build the image once when `docker_build` is enabled so refresh tests can
/// exercise the existing-image path without triggering extra rebuilds.
fn ensure_image() {
    if common::capability_enabled("docker_build") {
        BUILD_IMAGE.call_once(|| {
            let status = Command::new(env!("CARGO_BIN_EXE_claudecage"))
                .args(["image", "rebuild"])
                .status()
                .expect("failed to run claudecage image rebuild");
            assert!(status.success(), "claudecage image rebuild failed");
        });
    }
}

/// Remove the shared test image only when it actually exists.
fn remove_claudecage_image_if_present() {
    let inspect = Command::new("docker")
        .args(["image", "inspect", "claudecage:latest"])
        .output()
        .expect("failed to run docker image inspect");

    if !inspect.status.success() {
        let stderr = String::from_utf8_lossy(&inspect.stderr);
        assert!(
            stderr.contains("No such image"),
            "docker image inspect claudecage:latest failed unexpectedly: {stderr}"
        );
        return;
    }

    let remove = Command::new("docker")
        .args(["image", "rm", "-f", "claudecage:latest"])
        .status()
        .expect("failed to run docker image rm");
    assert!(remove.success(), "docker image rm claudecage:latest failed");
}

// --- docker capability tests ---

/// Verify that `docker image inspect` on a nonexistent image fails with "No such image".
///
/// This exercises the same error path that `image_exists` relies on to distinguish
/// "image not found" from docker daemon errors.
#[test]
fn inspect_missing_image_reports_no_such_image() {
    if !common::capability_enabled("docker") {
        return;
    }

    let output = Command::new("docker")
        .args(["image", "inspect", "claudecage-nonexistent:test"])
        .output()
        .expect("failed to run docker");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("No such image"),
        "expected 'No such image' in stderr, got: {stderr}"
    );
}

// --- docker_build capability tests ---

/// Build the claudecage Docker image from scratch and verify it exists afterward.
///
/// Only runs with the `docker_build` capability — skipped during fast local iteration
/// where the image is assumed to already exist.
#[test]
fn image_rebuild_builds_successfully() {
    if !common::capability_enabled("docker_build") {
        return;
    }
    let _guard = IMAGE_TEST_LOCK.lock().unwrap();

    ensure_image();

    let inspect = Command::new("docker")
        .args(["image", "inspect", "claudecage:latest"])
        .output()
        .expect("failed to run docker image inspect");

    assert!(
        inspect.status.success(),
        "claudecage:latest should exist after image rebuild"
    );
}

/// Verify that `image build --rebuild` preserves the full rebuild entrypoint.
#[test]
fn image_build_rebuild_builds_successfully() {
    if !common::capability_enabled("docker_build") {
        return;
    }
    let _guard = IMAGE_TEST_LOCK.lock().unwrap();

    let status = Command::new(env!("CARGO_BIN_EXE_claudecage"))
        .args(["image", "build", "--rebuild"])
        .status()
        .expect("failed to run claudecage image build --rebuild");
    assert!(status.success(), "claudecage image build --rebuild failed");

    let inspect = Command::new("docker")
        .args(["image", "inspect", "claudecage:latest"])
        .output()
        .expect("failed to run docker image inspect");

    assert!(
        inspect.status.success(),
        "claudecage:latest should exist after image build --rebuild"
    );
}

/// Refresh the image successfully when it already exists.
#[test]
fn image_refresh_succeeds_with_existing_image() {
    if !common::capability_enabled("docker_build") {
        return;
    }
    let _guard = IMAGE_TEST_LOCK.lock().unwrap();

    ensure_image();

    let status = Command::new(env!("CARGO_BIN_EXE_claudecage"))
        .args(["image", "refresh"])
        .status()
        .expect("failed to run claudecage image refresh");
    assert!(status.success(), "claudecage image refresh failed");

    let inspect = Command::new("docker")
        .args(["image", "inspect", "claudecage:latest"])
        .output()
        .expect("failed to run docker image inspect");

    assert!(
        inspect.status.success(),
        "claudecage:latest should exist after image refresh"
    );
}

/// Refresh the image successfully even when the expected tag is absent.
#[test]
fn image_refresh_builds_missing_image() {
    if !common::capability_enabled("docker_build") {
        return;
    }
    let _guard = IMAGE_TEST_LOCK.lock().unwrap();

    remove_claudecage_image_if_present();

    let status = Command::new(env!("CARGO_BIN_EXE_claudecage"))
        .args(["image", "refresh"])
        .status()
        .expect("failed to run claudecage image refresh");
    assert!(status.success(), "claudecage image refresh failed");

    let inspect = Command::new("docker")
        .args(["image", "inspect", "claudecage:latest"])
        .output()
        .expect("failed to run docker image inspect");

    assert!(
        inspect.status.success(),
        "claudecage:latest should exist after image refresh"
    );
}

/// Verify that `docker image inspect` succeeds on an image known to exist.
///
/// Uses `hello-world` which is tiny and widely available. Pulls it first to
/// ensure it's present.
#[test]
fn inspect_present_image_succeeds() {
    if !common::capability_enabled("docker") {
        return;
    }

    let pull = Command::new("docker")
        .args(["pull", "hello-world"])
        .output()
        .expect("failed to pull hello-world");
    assert!(pull.status.success(), "docker pull hello-world failed");

    let output = Command::new("docker")
        .args(["image", "inspect", "hello-world"])
        .output()
        .expect("failed to run docker");

    assert!(
        output.status.success(),
        "docker image inspect hello-world should succeed"
    );
}

// --- claude_auth capability tests ---

/// End-to-end test: run a single-turn claude prompt inside the container.
///
/// Requires claude to be authenticated and the container image to exist
/// (either pre-built or via `docker_build` capability).
#[test]
fn claude_responds_to_prompt() {
    if !common::capability_enabled("claude_auth") {
        return;
    }
    let _guard = IMAGE_TEST_LOCK.lock().unwrap();

    ensure_image();

    // Three things must be right here or the test hangs:
    //
    // 1. Must use `claudecage claude`, not `claudecage run`. `claudecage claude`
    //    adds --dangerously-skip-permissions and --settings to suppress the TOS
    //    prompt. Without these flags, claude shows an interactive workspace trust
    //    dialog that blocks forever when stdin is /dev/null.
    //
    // 2. All stdio must be Stdio::null(). Cargo test captures stdout via a pipe.
    //    claudecage uses cmd.status() for docker, which inherits the parent's
    //    file descriptors. If stdout/stderr are cargo's pipes and a subprocess
    //    inside the container inherits those fds and outlives docker, the pipe
    //    never gets EOF and .output() (or even .status() with inherited pipes)
    //    hangs forever.
    //
    // 3. claudecage must not pass -i or -it to docker when stdin is not a TTY.
    //    Docker's -i flag keeps the container's stdin open. If nothing closes it
    //    (because the parent's stdin is a pipe that cargo holds open), the
    //    container never exits. With Stdio::null(), claudecage sees stdin is not
    //    a TTY and omits -i entirely.
    let status = Command::new(env!("CARGO_BIN_EXE_claudecage"))
        .args(["claude", "--", "-p", "respond with exactly the word PING"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("failed to run claudecage claude");

    assert!(status.success(), "claude -p failed with {status}");
}
