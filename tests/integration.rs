mod common;

use std::process::Command;
use std::sync::Once;

// All integration tests live in one binary so this Once synchronizes image
// creation across tests that run in parallel within the binary.
static BUILD_IMAGE: Once = Once::new();

fn ensure_image() {
    BUILD_IMAGE.call_once(|| {
        let status = Command::new(env!("CARGO_BIN_EXE_claudecage"))
            .args(["image", "recreate"])
            .status()
            .expect("failed to run claudecage image recreate");
        assert!(status.success(), "claudecage image recreate failed");
    });
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

/// Build the claudecage Docker image from scratch and verify it exists afterward.
#[test]
fn image_recreate_builds_successfully() {
    if !common::capability_enabled("docker") {
        return;
    }

    ensure_image();

    let inspect = Command::new("docker")
        .args(["image", "inspect", "claudecage:latest"])
        .output()
        .expect("failed to run docker image inspect");

    assert!(
        inspect.status.success(),
        "claudecage:latest should exist after image recreate"
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
/// Requires claude to be authenticated and the container image to exist.
#[test]
fn claude_responds_to_prompt() {
    if !common::capability_enabled("claude_auth") {
        return;
    }

    ensure_image();

    let output = Command::new(env!("CARGO_BIN_EXE_claudecage"))
        .args(["claude", "--", "-p", "respond with exactly the word PING"])
        .output()
        .expect("failed to run claudecage claude");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "claude command failed.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("PING"),
        "expected PING in output, got: {stdout}"
    );
}
