#[cfg(feature = "keyring-store")]
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
