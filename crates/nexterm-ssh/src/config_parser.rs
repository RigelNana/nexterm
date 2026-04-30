//! Parser for ~/.ssh/config files.

/// Parsed SSH config entry.
#[derive(Debug, Clone)]
pub struct SshConfigEntry {
    pub host_pattern: String,
    pub hostname: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub identity_file: Option<String>,
    pub proxy_jump: Option<String>,
    pub forward_agent: bool,
}

/// Parse ~/.ssh/config and return all entries.
pub fn parse_ssh_config(content: &str) -> Vec<SshConfigEntry> {
    let mut entries = Vec::new();
    let mut current: Option<SshConfigEntry> = None;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some((key, value)) = line.split_once(char::is_whitespace) {
            let key = key.trim().to_lowercase();
            let value = value.trim().to_string();

            match key.as_str() {
                "host" => {
                    if let Some(entry) = current.take() {
                        entries.push(entry);
                    }
                    current = Some(SshConfigEntry {
                        host_pattern: value,
                        hostname: None,
                        port: None,
                        user: None,
                        identity_file: None,
                        proxy_jump: None,
                        forward_agent: false,
                    });
                }
                "hostname" => {
                    if let Some(ref mut entry) = current {
                        entry.hostname = Some(value);
                    }
                }
                "port" => {
                    if let Some(ref mut entry) = current {
                        entry.port = value.parse().ok();
                    }
                }
                "user" => {
                    if let Some(ref mut entry) = current {
                        entry.user = Some(value);
                    }
                }
                "identityfile" => {
                    if let Some(ref mut entry) = current {
                        entry.identity_file = Some(value);
                    }
                }
                "proxyjump" => {
                    if let Some(ref mut entry) = current {
                        entry.proxy_jump = Some(value);
                    }
                }
                "forwardagent" => {
                    if let Some(ref mut entry) = current {
                        entry.forward_agent = value.to_lowercase() == "yes";
                    }
                }
                _ => {} // ignore unknown directives for now
            }
        }
    }

    if let Some(entry) = current {
        entries.push(entry);
    }

    entries
}
