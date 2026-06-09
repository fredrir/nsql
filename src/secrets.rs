//! Credential storage via the OS keychain (Secret Service / libsecret on Linux,
//! macOS Keychain, Windows Credential Manager). Plaintext passwords never touch
//! the config file. Feature-gated behind `keyring-store` so the tool still
//! builds where the secret-service stack is unavailable.
//!
//! Resolution order for an actual connection password (Phase 2, once non-sqlite
//! backends are wired) should be: explicit URI > env (PG*/DATABASE_URL) >
//! ~/.pgpass > OS keyring (here) > external managers (op://, pass).

const SERVICE: &str = "nsql";

#[cfg(feature = "keyring-store")]
pub fn set(profile: &str, password: &str) -> anyhow::Result<()> {
    let entry = keyring::Entry::new(SERVICE, profile)?;
    entry.set_password(password)?;
    Ok(())
}

#[cfg(feature = "keyring-store")]
#[allow(dead_code)]
pub fn get(profile: &str) -> Option<String> {
    keyring::Entry::new(SERVICE, profile)
        .ok()?
        .get_password()
        .ok()
}

#[cfg(not(feature = "keyring-store"))]
pub fn set(_profile: &str, _password: &str) -> anyhow::Result<()> {
    anyhow::bail!(
        "this build has no keyring support (compiled with --no-default-features); \
         store the secret in an env var / ~/.pgpass instead"
    )
}

#[cfg(not(feature = "keyring-store"))]
#[allow(dead_code)]
pub fn get(_profile: &str) -> Option<String> {
    None
}
