/// Check whether a test capability is enabled via `CLAUDECAGE_TEST_CAPABILITIES`.
///
/// The env var is a comma-separated list of capability names (e.g., "docker,keychain").
/// Tests that require a capability should call this at the top and return early if false.
pub fn capability_enabled(name: &str) -> bool {
    std::env::var("CLAUDECAGE_TEST_CAPABILITIES")
        .map(|v| v.split(',').any(|b| b.trim() == name))
        .unwrap_or(false)
}
