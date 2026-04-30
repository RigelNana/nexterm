//! # nexterm-keystore
//!
//! Secure credential storage: OS Keychain abstraction, SSH key management.

use anyhow::Result;
use keyring::Entry;
use tracing::info;

const SERVICE_NAME: &str = "nexterm";

/// Store a secret (password or passphrase) in the OS keychain.
pub fn store_secret(key: &str, secret: &str) -> Result<()> {
    let entry = Entry::new(SERVICE_NAME, key)?;
    entry.set_password(secret)?;
    info!(key = %key, "secret stored in keychain");
    Ok(())
}

/// Retrieve a secret from the OS keychain.
pub fn get_secret(key: &str) -> Result<Option<String>> {
    let entry = Entry::new(SERVICE_NAME, key)?;
    match entry.get_password() {
        Ok(pw) => Ok(Some(pw)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Delete a secret from the OS keychain.
pub fn delete_secret(key: &str) -> Result<()> {
    let entry = Entry::new(SERVICE_NAME, key)?;
    entry.delete_credential()?;
    info!(key = %key, "secret deleted from keychain");
    Ok(())
}

/// List available SSH private key files from ~/.ssh/.
pub fn discover_ssh_keys() -> Result<Vec<std::path::PathBuf>> {
    let ssh_dir = dirs::home_dir()
        .map(|h| h.join(".ssh"))
        .ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;

    let mut keys = Vec::new();
    if ssh_dir.exists() {
        for entry in std::fs::read_dir(&ssh_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                // Heuristic: private keys don't have .pub extension and match common names
                if !name.ends_with(".pub")
                    && !name.starts_with("known_hosts")
                    && !name.starts_with("config")
                    && !name.starts_with("authorized")
                {
                    keys.push(path);
                }
            }
        }
    }
    Ok(keys)
}
