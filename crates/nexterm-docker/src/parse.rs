//! Parser for `docker ps --format '{{json .}}'` output.
//!
//! Docker emits one JSON object per container, one per line. We deserialize
//! that raw shape via [`PsRawEntry`] and convert to the friendlier
//! [`ContainerInfo`] model.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::model::{ContainerInfo, ContainerStatus, PortMapping};

/// Exact shape of one line in `docker ps --format '{{json .}}'` output.
/// Fields are Pascal-case as Docker emits them.
#[derive(Debug, Deserialize)]
struct PsRawEntry {
    #[serde(rename = "ID")]
    id: String,
    #[serde(default, rename = "Names")]
    names: String,
    #[serde(default, rename = "Image")]
    image: String,
    #[serde(default, rename = "Command")]
    command: String,
    #[serde(default, rename = "CreatedAt")]
    created_at: String,
    #[serde(default, rename = "Status")]
    status: String,
    #[serde(default, rename = "State")]
    state: String,
    #[serde(default, rename = "Ports")]
    ports: String,
    #[serde(default, rename = "Size")]
    size: String,
    #[serde(default, rename = "Labels")]
    labels: String,
}

/// Parse the full output of `docker ps --format '{{json .}}'`.
///
/// Empty / whitespace-only lines are skipped. Any invalid JSON line is an
/// error — we don't want to silently hide containers.
pub fn parse_ps_lines(output: &str) -> Result<Vec<ContainerInfo>> {
    let mut out = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let raw: PsRawEntry = serde_json::from_str(trimmed)
            .with_context(|| format!("invalid docker ps JSON line: {trimmed}"))?;
        out.push(convert(raw));
    }
    Ok(out)
}

fn convert(r: PsRawEntry) -> ContainerInfo {
    let status = parse_status(&r.state, &r.status);
    ContainerInfo {
        id: r.id,
        names: split_names(&r.names),
        image: r.image,
        command: r.command,
        created_at: r.created_at,
        status,
        status_raw: r.status,
        ports: parse_ports(&r.ports),
        size: r.size,
        labels: parse_labels(&r.labels),
    }
}

fn split_names(s: &str) -> Vec<String> {
    s.split(',')
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect()
}

fn parse_status(state: &str, status: &str) -> ContainerStatus {
    // Modern (Docker >= 23.0) emits a `State` field directly. Match
    // case-insensitively for safety.
    match state.to_ascii_lowercase().as_str() {
        "running" => return ContainerStatus::Running,
        "paused" => return ContainerStatus::Paused,
        "restarting" => return ContainerStatus::Restarting,
        "created" => return ContainerStatus::Created,
        "removing" => return ContainerStatus::Removing,
        "dead" => return ContainerStatus::Dead,
        "exited" => {
            return ContainerStatus::Exited {
                code: parse_exit_code(status),
            };
        }
        "" => { /* fall through to status-field heuristic */ }
        other => return ContainerStatus::Unknown(other.to_string()),
    }

    // Older Docker (< 23.0) doesn't emit `.State`. Derive the status from
    // the human-readable `.Status` column instead. Examples:
    //   "Up 3 hours"
    //   "Up 3 hours (Paused)"
    //   "Up 3 hours (unhealthy)"
    //   "Restarting (1) 5 seconds ago"
    //   "Exited (0) 2 days ago"
    //   "Created"
    //   "Dead"
    //   "Removal In Progress"
    if status.is_empty() {
        return ContainerStatus::Unknown(String::new());
    }
    if status.contains("(Paused)") {
        return ContainerStatus::Paused;
    }
    if status.starts_with("Up ") || status == "Up" {
        // "Up 3 hours (unhealthy)" still counts as running.
        return ContainerStatus::Running;
    }
    if status.starts_with("Restarting") {
        return ContainerStatus::Restarting;
    }
    if status.starts_with("Exited") {
        return ContainerStatus::Exited {
            code: parse_exit_code(status),
        };
    }
    if status.starts_with("Created") {
        return ContainerStatus::Created;
    }
    if status.starts_with("Dead") {
        return ContainerStatus::Dead;
    }
    if status.starts_with("Removal In Progress") || status.starts_with("Removing") {
        return ContainerStatus::Removing;
    }
    ContainerStatus::Unknown(status.to_string())
}

/// Extract the exit code from a status string like `"Exited (137) 5 minutes ago"`.
fn parse_exit_code(status: &str) -> Option<i32> {
    let after = status.strip_prefix("Exited (")?;
    let (code, _) = after.split_once(')')?;
    code.trim().parse().ok()
}

fn parse_labels(s: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for pair in s.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        if let Some((k, v)) = pair.split_once('=') {
            out.insert(k.to_string(), v.to_string());
        } else {
            out.insert(pair.to_string(), String::new());
        }
    }
    out
}

fn parse_ports(s: &str) -> Vec<PortMapping> {
    s.split(',')
        .filter_map(|p| parse_single_port(p.trim()))
        .collect()
}

/// Parse one of:
/// * `"0.0.0.0:5432->5432/tcp"`
/// * `"[::]:5432->5432/tcp"`
/// * `"5432/tcp"`      (internal only — no host side)
fn parse_single_port(s: &str) -> Option<PortMapping> {
    if s.is_empty() {
        return None;
    }

    let (left, right) = match s.split_once("->") {
        Some((l, r)) => (Some(l.trim()), r.trim()),
        None => (None, s),
    };

    // `right` is always `<containerPort>/<proto>`.
    let (cport_s, proto) = right.split_once('/')?;
    let container_port: u16 = cport_s.parse().ok()?;

    let (host_ip, host_port) = match left {
        Some(l) => parse_host_binding(l),
        None => (None, None),
    };

    Some(PortMapping {
        host_ip,
        host_port,
        container_port,
        protocol: proto.to_string(),
    })
}

/// Split `"0.0.0.0:5432"` / `"[::]:5432"` / `":::5432"` into (ip, port).
fn parse_host_binding(s: &str) -> (Option<String>, Option<u16>) {
    let (ip, port) = match s.rsplit_once(':') {
        Some(pair) => pair,
        None => return (None, None),
    };
    let ip = ip.trim_start_matches('[').trim_end_matches(']');
    let ip_opt = if ip.is_empty() {
        None
    } else {
        Some(ip.to_string())
    };
    (ip_opt, port.parse::<u16>().ok())
}

// ------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Realistic `docker ps -a --format '{{json .}}'` output with three
    /// containers in different states. Each line is one JSON object.
    const FIXTURE: &str = r#"
{"Command":"\"docker-entrypoint.s…\"","CreatedAt":"2025-01-15 10:23:45 +0800 CST","ID":"abc123def4567890","Image":"postgres:15","Labels":"com.example.app=db,env=prod","LocalVolumes":"1","Mounts":"pg_data","Names":"postgres-main","Networks":"bridge","Ports":"0.0.0.0:5432->5432/tcp, [::]:5432->5432/tcp","RunningFor":"3 hours ago","Size":"0B","State":"running","Status":"Up 3 hours"}
{"Command":"\"/docker-entrypoint.…\"","CreatedAt":"2025-01-10 08:00:00 +0800 CST","ID":"feedfacecafe0000","Image":"nginx:latest","Labels":"","LocalVolumes":"0","Mounts":"","Names":"nginx-web,alias","Networks":"bridge","Ports":"80/tcp","RunningFor":"5 days ago","Size":"","State":"exited","Status":"Exited (137) 2 minutes ago"}
{"Command":"\"redis-server\"","CreatedAt":"2025-01-12 12:00:00 +0800 CST","ID":"cafebabe1234abcd","Image":"redis:alpine","Labels":"","LocalVolumes":"0","Mounts":"","Names":"redis-cache","Networks":"bridge","Ports":"","RunningFor":"3 days ago","Size":"","State":"paused","Status":"Up 3 days (Paused)"}
"#;

    #[test]
    fn parses_realistic_fixture() {
        let containers = parse_ps_lines(FIXTURE).unwrap();
        assert_eq!(containers.len(), 3);

        let pg = &containers[0];
        assert_eq!(pg.id, "abc123def4567890");
        assert_eq!(pg.short_id(), "abc123def456");
        assert_eq!(pg.names, vec!["postgres-main".to_string()]);
        assert_eq!(pg.image, "postgres:15");
        assert_eq!(pg.status, ContainerStatus::Running);
        assert!(pg.status.is_running());
        assert_eq!(pg.ports.len(), 2);
        assert_eq!(pg.ports[0].host_ip.as_deref(), Some("0.0.0.0"));
        assert_eq!(pg.ports[0].host_port, Some(5432));
        assert_eq!(pg.ports[0].container_port, 5432);
        assert_eq!(pg.ports[0].protocol, "tcp");
        assert_eq!(pg.ports[1].host_ip.as_deref(), Some("::"));
        assert_eq!(
            pg.labels.get("com.example.app").map(String::as_str),
            Some("db")
        );
        assert_eq!(pg.labels.get("env").map(String::as_str), Some("prod"));

        let nginx = &containers[1];
        assert_eq!(
            nginx.names,
            vec!["nginx-web".to_string(), "alias".to_string()]
        );
        assert_eq!(nginx.status, ContainerStatus::Exited { code: Some(137) });
        assert!(nginx.status.is_stopped());
        // Internal-only port: no host side.
        assert_eq!(nginx.ports.len(), 1);
        assert!(nginx.ports[0].host_ip.is_none());
        assert!(nginx.ports[0].host_port.is_none());
        assert_eq!(nginx.ports[0].container_port, 80);

        let redis = &containers[2];
        assert_eq!(redis.status, ContainerStatus::Paused);
        assert_eq!(redis.ports.len(), 0);
    }

    #[test]
    fn skips_blank_lines() {
        let input = "\n\n   \n";
        assert!(parse_ps_lines(input).unwrap().is_empty());
    }

    #[test]
    fn errors_on_invalid_json() {
        let err = parse_ps_lines("not json at all").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("invalid docker ps JSON line"));
    }

    #[test]
    fn parses_unknown_state() {
        let line = r#"{"ID":"x","Names":"x","Image":"x","State":"weird","Status":"???"}"#;
        let c = &parse_ps_lines(line).unwrap()[0];
        assert!(matches!(&c.status, ContainerStatus::Unknown(s) if s == "weird"));
    }

    #[test]
    fn parses_exited_without_code() {
        let line = r#"{"ID":"x","Names":"x","Image":"x","State":"exited","Status":"Exited"}"#;
        let c = &parse_ps_lines(line).unwrap()[0];
        assert_eq!(c.status, ContainerStatus::Exited { code: None });
    }

    #[test]
    fn display_name_falls_back_to_id() {
        let line =
            r#"{"ID":"abcdef123456","Names":"","Image":"x","State":"running","Status":"Up"}"#;
        let c = &parse_ps_lines(line).unwrap()[0];
        assert_eq!(c.display_name(), "abcdef123456");
    }

    #[test]
    fn port_binding_ipv6_bracketed() {
        // Bracketed [::] is Docker's convention for IPv6 any-address.
        let pm = parse_single_port("[::]:8080->80/tcp").unwrap();
        assert_eq!(pm.host_ip.as_deref(), Some("::"));
        assert_eq!(pm.host_port, Some(8080));
        assert_eq!(pm.container_port, 80);
    }

    /// Older docker (< 23.0) over SSH didn't emit the `State` field at all.
    /// We must fall back to parsing the human `Status` column.
    #[test]
    fn legacy_docker_without_state_field() {
        // No "State" key — modeled after Docker 20.10 output.
        let lines = "\
{\"ID\":\"a\",\"Names\":\"web\",\"Image\":\"nginx\",\"Status\":\"Up 3 hours\"}
{\"ID\":\"b\",\"Names\":\"db\",\"Image\":\"postgres\",\"Status\":\"Exited (137) 2 days ago\"}
{\"ID\":\"c\",\"Names\":\"r\",\"Image\":\"redis\",\"Status\":\"Up 2 minutes (Paused)\"}
{\"ID\":\"d\",\"Names\":\"x\",\"Image\":\"x\",\"Status\":\"Restarting (1) 5 seconds ago\"}
{\"ID\":\"e\",\"Names\":\"y\",\"Image\":\"y\",\"Status\":\"Up 1 hour (unhealthy)\"}
";
        let v = parse_ps_lines(lines).unwrap();
        assert_eq!(v[0].status, ContainerStatus::Running);
        assert_eq!(v[1].status, ContainerStatus::Exited { code: Some(137) });
        assert_eq!(v[2].status, ContainerStatus::Paused);
        assert_eq!(v[3].status, ContainerStatus::Restarting);
        // unhealthy is still running
        assert_eq!(v[4].status, ContainerStatus::Running);
    }

    #[test]
    fn state_field_is_case_insensitive() {
        // Defensive: just in case some forks emit Title-case state strings.
        let line = r#"{"ID":"x","Names":"x","Image":"x","State":"Running","Status":"Up"}"#;
        assert_eq!(
            parse_ps_lines(line).unwrap()[0].status,
            ContainerStatus::Running,
        );
    }
}
