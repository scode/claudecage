//! macOS Keychain storage for GitHub tokens.
//!
//! Tokens are stored and retrieved via the `/usr/bin/security` CLI to avoid
//! Keychain access prompts that would occur with direct framework API calls
//! (each recompiled binary gets a new code signature).
use std::process::Command;

use anyhow::{bail, Context, Result};
use tracing::debug;

const KEYCHAIN_SERVICE: &str = "claudecage";
const KEYCHAIN_ACCOUNT: &str = "github-token";

/// Validate that a token looks like a GitHub PAT (correct prefix + non-empty body).
pub fn validate_github_token(token: &str) -> Result<()> {
    let body = token
        .strip_prefix("ghp_")
        .or_else(|| token.strip_prefix("github_pat_"));
    match body {
        Some(b) if !b.is_empty() => Ok(()),
        _ => bail!("token must be a GitHub PAT starting with 'ghp_' or 'github_pat_' followed by token characters"),
    }
}

/// Store a GitHub token in the macOS Keychain, replacing any existing entry.
///
/// The token never appears in process argument lists.
pub fn store_github_token(token: &str) -> Result<()> {
    use std::io::Write;

    let mut child = Command::new("/usr/bin/security")
        .args([
            "add-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            KEYCHAIN_ACCOUNT,
            "-U",
            "-w",
        ])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("failed to run security add-generic-password")?;

    // Written twice because `-w` without a value prompts for password + confirmation.
    let stdin = child.stdin.as_mut().context("failed to open stdin pipe")?;
    writeln!(stdin, "{token}").context("failed to write token")?;
    writeln!(stdin, "{token}").context("failed to write token confirmation")?;
    drop(child.stdin.take());

    let status = child.wait().context("failed to wait for security")?;
    if !status.success() {
        bail!("failed to store token in keychain (security exited with {status})");
    }

    Ok(())
}

/// Remove the stored GitHub token from the macOS Keychain.
///
/// Returns Ok(()) whether or not a token was previously stored.
pub fn remove_github_token() -> Result<()> {
    let status = Command::new("/usr/bin/security")
        .args([
            "delete-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            KEYCHAIN_ACCOUNT,
        ])
        .status()
        .context("failed to run security delete-generic-password")?;

    // Exit code 44 means the item wasn't found — that's fine.
    if !status.success() && status.code() != Some(44) {
        bail!("failed to remove token from keychain (security exited with {status})");
    }

    Ok(())
}

/// Interpret the output of `security find-generic-password -w`.
///
/// - Success with non-empty stdout: returns the trimmed token.
/// - Success with empty/whitespace stdout: returns None (treat as unset).
/// - Exit code 44 (item not found): returns None.
/// - Any other failure: returns an error.
fn parse_keychain_output(exit_code: Option<i32>, stdout: &[u8]) -> Result<Option<String>> {
    match exit_code {
        Some(0) => {
            let token = String::from_utf8(stdout.to_vec())
                .context("keychain token is not valid UTF-8")?
                .trim()
                .to_string();
            if token.is_empty() {
                Ok(None)
            } else if token.contains('\n') || token.contains('\r') {
                bail!("keychain token contains newline characters")
            } else {
                Ok(Some(token))
            }
        }
        Some(44) => Ok(None),
        other => bail!(
            "failed to read token from keychain (security exited with {})",
            other.map_or("signal".to_string(), |c| c.to_string())
        ),
    }
}

/// Look up the GitHub token from the macOS Keychain.
///
/// Returns Ok(None) if no token is stored or if the stored value is empty.
pub fn resolve_github_token() -> Result<Option<String>> {
    let output = Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            KEYCHAIN_ACCOUNT,
            "-w",
        ])
        .output()
        .context("failed to run security find-generic-password")?;

    let result = parse_keychain_output(output.status.code(), &output.stdout)?;
    match &result {
        Some(_) => debug!("found GitHub token in keychain"),
        None => debug!("no GitHub token in keychain"),
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_classic_token() {
        validate_github_token("ghp_abc123").unwrap();
    }

    #[test]
    fn validate_accepts_fine_grained_token() {
        validate_github_token("github_pat_abc123").unwrap();
    }

    #[test]
    fn validate_rejects_bare_prefix() {
        assert!(validate_github_token("ghp_").is_err());
        assert!(validate_github_token("github_pat_").is_err());
    }

    #[test]
    fn validate_rejects_no_prefix() {
        assert!(validate_github_token("abc123").is_err());
    }

    #[test]
    fn validate_rejects_empty() {
        assert!(validate_github_token("").is_err());
    }

    #[test]
    fn validate_rejects_near_miss() {
        assert!(validate_github_token("ghp").is_err());
        assert!(validate_github_token("github_pat").is_err());
    }

    #[test]
    fn parse_keychain_success_returns_token() {
        let result = parse_keychain_output(Some(0), b"ghp_abc123\n").unwrap();
        assert_eq!(result.as_deref(), Some("ghp_abc123"));
    }

    #[test]
    fn parse_keychain_success_trims_whitespace() {
        let result = parse_keychain_output(Some(0), b"  ghp_abc123  \n").unwrap();
        assert_eq!(result.as_deref(), Some("ghp_abc123"));
    }

    #[test]
    fn parse_keychain_success_empty_returns_none() {
        assert_eq!(parse_keychain_output(Some(0), b"").unwrap(), None);
        assert_eq!(parse_keychain_output(Some(0), b"  \n").unwrap(), None);
    }

    #[test]
    fn parse_keychain_not_found_returns_none() {
        assert_eq!(parse_keychain_output(Some(44), b"").unwrap(), None);
    }

    #[test]
    fn parse_keychain_other_error_returns_err() {
        assert!(parse_keychain_output(Some(1), b"").is_err());
        assert!(parse_keychain_output(Some(2), b"").is_err());
        assert!(parse_keychain_output(None, b"").is_err());
    }

    #[test]
    fn parse_keychain_rejects_embedded_newline() {
        assert!(parse_keychain_output(Some(0), b"ghp_abc\nEVIL=1\n").is_err());
        assert!(parse_keychain_output(Some(0), b"ghp_abc\r\nEVIL=1\n").is_err());
    }

    #[test]
    fn parse_keychain_rejects_non_utf8() {
        assert!(parse_keychain_output(Some(0), b"\xff\xfe").is_err());
    }
}
