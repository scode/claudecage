mod common;

use std::process::Command;

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

    let status = Command::new(env!("CARGO_BIN_EXE_claudecage"))
        .args(["image", "recreate"])
        .status()
        .expect("failed to run claudecage image recreate");

    assert!(status.success(), "claudecage image recreate failed");

    let inspect = Command::new("docker")
        .args(["image", "inspect", "claudecage:latest"])
        .output()
        .expect("failed to run docker image inspect");

    assert!(
        inspect.status.success(),
        "claudecage:latest should exist after image create"
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

    // Ensure the image is present.
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
