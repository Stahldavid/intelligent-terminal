//! OpenSSH-backed discovery, resolution and fail-closed health probes.
//!
//! We only parse configuration to enumerate concrete aliases. `ssh -G` remains
//! the authority for precedence, Include/Match evaluation and final options.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::model::*;
use super::store::now_ms;

const MAX_INCLUDE_DEPTH: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedSshTarget {
    pub alias: String,
    pub hostname: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    pub port: u16,
    #[serde(default)]
    pub identity_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_jump: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_command: Option<String>,
    #[serde(default)]
    pub effective_options: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SshProbeResult {
    pub alias: String,
    pub health: TargetHealth,
    pub resolved: ResolvedSshTarget,
    pub latency_ms: u64,
    pub checked_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTrustPreview {
    pub alias: String,
    pub hostname: String,
    pub port: u16,
    pub fingerprints: Vec<String>,
    pub command_preview: String,
    pub writes_known_hosts: bool,
}

pub fn find_ssh_executable() -> Result<PathBuf> {
    which::which("ssh.exe")
        .or_else(|_| which::which("ssh"))
        .context("OpenSSH client was not found on PATH")
}

pub fn discover_aliases() -> Result<Vec<String>> {
    let mut aliases = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for path in default_config_paths() {
        enumerate_config(&path, 0, &mut visited, &mut aliases)?;
    }
    Ok(aliases.into_iter().collect())
}

pub fn discover_targets() -> Result<Vec<ComputeTarget>> {
    let mut targets = Vec::new();
    for alias in discover_aliases()? {
        let resolved = match resolve_alias(&alias) {
            Ok(value) => value,
            Err(_) => continue,
        };
        targets.push(ComputeTarget {
            schema_version: COMPUTE_SCHEMA_VERSION,
            id: format!("ssh:{}", slug(&alias)),
            display_name: format!("SSH — {alias}"),
            provider: ProviderKind::Ssh,
            endpoint: TargetEndpoint {
                ssh_alias: Some(alias.clone()),
                ..Default::default()
            },
            os: "unknown".to_string(),
            arch: "unknown".to_string(),
            capabilities: vec!["remote_shell".to_string(), "wta_node".to_string()],
            toolchains: BTreeMap::new(),
            // Discovery proves only that an alias exists and resolves. It does
            // not prove owner intent, host-key trust or workload suitability.
            trust_tier: TrustTier::Restricted,
            project_allowlist: Vec::new(),
            agent_slots: 1,
            build_slots: 1,
            memory_bytes: 0,
            cost_policy: serde_json::Value::Null,
            power_policy: serde_json::Value::Null,
            health: TargetHealth::Unknown,
            last_probe_at_ms: None,
            disabled: true,
            metadata: json!({
                "resolved_hostname": resolved.hostname,
                "resolved_user": resolved.user,
                "resolved_port": resolved.port,
                "proxy_jump": resolved.proxy_jump,
            }),
        });
    }
    Ok(targets)
}

pub fn resolve_alias(alias: &str) -> Result<ResolvedSshTarget> {
    validate_alias(alias)?;
    let output = Command::new(find_ssh_executable()?)
        .arg("-G")
        .arg(alias)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to run ssh -G {alias}"))?;
    if !output.status.success() {
        bail!(
            "ssh -G failed for {alias}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_ssh_g(alias, &String::from_utf8_lossy(&output.stdout))
}

pub fn probe(alias: &str, accept_new_host_key: bool) -> Result<SshProbeResult> {
    validate_alias(alias)?;
    let resolved = resolve_alias(alias)?;
    let started = Instant::now();
    let mut command = Command::new(find_ssh_executable()?);
    command
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=5")
        .arg("-o")
        .arg("ServerAliveInterval=15")
        .arg("-o")
        .arg("ServerAliveCountMax=2");
    if accept_new_host_key {
        // Only reached from the explicit `target trust` operation.
        command.arg("-o").arg("StrictHostKeyChecking=accept-new");
    }
    let output = command
        .arg(alias)
        .arg("--")
        .arg("printf")
        .arg("wta-ssh-probe")
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to probe SSH target {alias}"))?;
    let elapsed = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (health, error) = classify_probe(&output, &stderr, stdout.contains("wta-ssh-probe"));
    Ok(SshProbeResult {
        alias: alias.to_string(),
        health,
        resolved,
        latency_ms: elapsed,
        checked_at_ms: now_ms(),
        error,
    })
}

pub fn preview_trust(alias: &str) -> Result<SshTrustPreview> {
    let resolved = resolve_alias(alias)?;
    if resolved.hostname.starts_with('-')
        || resolved
            .hostname
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        bail!("resolved SSH hostname is unsafe for key preview");
    }
    let keygen = which::which("ssh-keygen.exe")
        .or_else(|_| which::which("ssh-keygen"))
        .context("OpenSSH ssh-keygen was not found on PATH")?;
    // Prefer the exact key that the configured OpenSSH client already
    // trusts. This honors HostKeyAlias and UserKnownHostsFile, works with
    // hashed known_hosts files, and avoids an ssh-keyscan compatibility gap
    // seen between the Windows client and newer OpenSSH KEX advertisements.
    let known = known_host_keys(&resolved, &keygen)?;
    let scanned = if known.is_empty() {
        let keyscan = which::which("ssh-keyscan.exe")
            .or_else(|_| which::which("ssh-keyscan"))
            .context("OpenSSH ssh-keyscan was not found on PATH")?;
        let scanned = Command::new(keyscan)
            .arg("-T")
            .arg("5")
            .arg("-p")
            .arg(resolved.port.to_string())
            .arg(&resolved.hostname)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("failed to scan SSH host key for {alias}"))?;
        if !scanned.status.success() || scanned.stdout.is_empty() {
            bail!(
                "SSH host key preview failed for {alias}; the key is not already trusted and ssh-keyscan is incompatible or unavailable: {}. Connect once with the system OpenSSH client, verify the fingerprint out of band, then retry",
                String::from_utf8_lossy(&scanned.stderr).trim()
            );
        }
        scanned.stdout
    } else {
        known
    };
    let mut fingerprint = Command::new(keygen)
        .arg("-lf")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start ssh-keygen for host-key fingerprinting")?;
    fingerprint
        .stdin
        .as_mut()
        .context("ssh-keygen stdin is unavailable")?
        .write_all(&scanned)?;
    let output = fingerprint.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "SSH host-key fingerprinting failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let fingerprints = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if fingerprints.is_empty() {
        bail!("SSH host-key scan returned no fingerprints");
    }
    Ok(SshTrustPreview {
        alias: alias.to_string(),
        hostname: resolved.hostname,
        port: resolved.port,
        fingerprints,
        command_preview: format!("ssh -o StrictHostKeyChecking=accept-new {alias}"),
        writes_known_hosts: false,
    })
}

fn known_host_keys(resolved: &ResolvedSshTarget, keygen: &Path) -> Result<Vec<u8>> {
    let host_key_alias = resolved
        .effective_options
        .get("hostkeyalias")
        .and_then(|values| values.first())
        .filter(|value| value.as_str() != "none");
    let lookup = host_key_alias.cloned().unwrap_or_else(|| {
        if resolved.port == 22 {
            resolved.hostname.clone()
        } else {
            format!("[{}]:{}", resolved.hostname, resolved.port)
        }
    });
    let paths = resolved
        .effective_options
        .get("userknownhostsfile")
        .into_iter()
        .flatten()
        .flat_map(|value| value.split_ascii_whitespace())
        .map(expand_home)
        .collect::<Vec<_>>();
    let mut keys = Vec::new();
    for path in paths {
        if !path.is_file() {
            continue;
        }
        let output = Command::new(keygen)
            .arg("-F")
            .arg(&lookup)
            .arg("-f")
            .arg(&path)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("failed to inspect known-hosts file {}", path.display()))?;
        if !output.status.success() {
            continue;
        }
        for line in output.stdout.split(|byte| *byte == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() || line.starts_with(b"#") {
                continue;
            }
            keys.extend_from_slice(line);
            keys.push(b'\n');
        }
    }
    Ok(keys)
}

/// Resolve the remote OS/architecture using fixed argv after SSH trust has
/// already succeeded. No user-provided command fragment is accepted.
pub fn probe_platform(alias: &str) -> Result<(String, String)> {
    validate_alias(alias)?;
    let output = Command::new(find_ssh_executable()?)
        .arg(alias)
        .arg("--")
        .arg("uname")
        .arg("-s")
        .arg("-m")
        .stdin(Stdio::null())
        .output()?;
    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout);
        let mut fields = text.split_ascii_whitespace();
        let os = fields.next().unwrap_or("unknown").to_ascii_lowercase();
        let arch = normalize_arch(fields.next().unwrap_or("unknown"));
        return Ok((os, arch));
    }
    let output = Command::new(find_ssh_executable()?)
        .arg(alias)
        .arg("--")
        .arg("powershell.exe")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-Command")
        .arg("[System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()")
        .stdin(Stdio::null())
        .output()?;
    if output.status.success() {
        return Ok((
            "windows".to_string(),
            normalize_arch(String::from_utf8_lossy(&output.stdout).trim()),
        ));
    }
    bail!("could not determine remote platform for {alias}")
}

fn classify_probe(output: &Output, stderr: &str, marker: bool) -> (TargetHealth, Option<String>) {
    if output.status.success() && marker {
        return (TargetHealth::Healthy, None);
    }
    let lower = stderr.to_ascii_lowercase();
    let health = if lower.contains("remote host identification has changed") {
        TargetHealth::HostKeyChanged
    } else if lower.contains("host key verification failed")
        || lower.contains("authenticity of host")
    {
        TargetHealth::TrustRequired
    } else {
        TargetHealth::Unreachable
    };
    (health, (!stderr.is_empty()).then(|| stderr.to_string()))
}

fn parse_ssh_g(alias: &str, text: &str) -> Result<ResolvedSshTarget> {
    let mut values: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.trim().split_once(char::is_whitespace) else {
            continue;
        };
        let value = value.trim();
        if !value.is_empty() {
            values
                .entry(key.to_ascii_lowercase())
                .or_default()
                .push(value.to_string());
        }
    }
    let first = |key: &str| values.get(key).and_then(|items| items.first()).cloned();
    let hostname = first("hostname").context("ssh -G did not return hostname")?;
    let port = first("port")
        .unwrap_or_else(|| "22".to_string())
        .parse::<u16>()
        .context("ssh -G returned invalid port")?;
    Ok(ResolvedSshTarget {
        alias: alias.to_string(),
        hostname,
        user: first("user"),
        port,
        identity_files: values.get("identityfile").cloned().unwrap_or_default(),
        proxy_jump: first("proxyjump").filter(|value| value != "none"),
        proxy_command: first("proxycommand").filter(|value| value != "none"),
        effective_options: values,
    })
}

fn default_config_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(program_data) = std::env::var_os("PROGRAMDATA") {
        paths.push(PathBuf::from(program_data).join("ssh").join("ssh_config"));
    }
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        paths.push(PathBuf::from(profile).join(".ssh").join("config"));
    } else if let Some(home) = std::env::var_os("HOME") {
        paths.push(PathBuf::from(home).join(".ssh").join("config"));
    }
    paths
}

fn enumerate_config(
    path: &Path,
    depth: usize,
    visited: &mut BTreeSet<PathBuf>,
    aliases: &mut BTreeSet<String>,
) -> Result<()> {
    if depth > MAX_INCLUDE_DEPTH || !path.is_file() {
        return Ok(());
    }
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical) {
        return Ok(());
    }
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read SSH config {}", path.display()))?;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if key.eq_ignore_ascii_case("host") {
            for alias in value.split_ascii_whitespace() {
                if is_concrete_alias(alias) {
                    aliases.insert(alias.to_string());
                }
            }
        } else if key.eq_ignore_ascii_case("include") {
            for pattern in value.split_ascii_whitespace() {
                let expanded = expand_home(pattern);
                let resolved = if expanded.is_absolute() {
                    expanded
                } else {
                    path.parent()
                        .unwrap_or_else(|| Path::new("."))
                        .join(expanded)
                };
                for include in expand_path_pattern(&resolved)? {
                    enumerate_config(&include, depth + 1, visited, aliases)?;
                }
            }
        }
    }
    Ok(())
}

fn expand_home(value: &str) -> PathBuf {
    if value == "~" || value.starts_with("~/") || value.starts_with("~\\") {
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .unwrap_or_else(|| OsString::from("."));
        return if value.len() == 1 {
            PathBuf::from(home)
        } else {
            PathBuf::from(home).join(&value[2..])
        };
    }
    PathBuf::from(value)
}

fn expand_path_pattern(path: &Path) -> Result<Vec<PathBuf>> {
    let text = path.to_string_lossy();
    if !text.contains(['*', '?']) {
        return Ok(path
            .is_file()
            .then(|| path.to_path_buf())
            .into_iter()
            .collect());
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let pattern = path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut matches = Vec::new();
    if parent.is_dir() {
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            if wildcard_match(&pattern, &entry.file_name().to_string_lossy())
                && entry.path().is_file()
            {
                matches.push(entry.path());
            }
        }
    }
    matches.sort();
    Ok(matches)
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let (mut p, mut v, mut star, mut mark) = (0, 0, None, 0);
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p].eq_ignore_ascii_case(&value[v])) {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            mark = v;
        } else if let Some(star_index) = star {
            p = star_index + 1;
            mark += 1;
            v = mark;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

pub fn validate_alias(alias: &str) -> Result<()> {
    if !is_concrete_alias(alias)
        || alias.starts_with('-')
        || alias
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        bail!("invalid concrete SSH alias: {alias:?}");
    }
    Ok(())
}

fn is_concrete_alias(alias: &str) -> bool {
    !alias.is_empty()
        && !alias.starts_with('!')
        && !alias.contains('*')
        && !alias.contains('?')
        && !alias.contains('[')
}

fn slug(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

fn normalize_arch(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "x64" | "x86_64" | "amd64" => "x86_64".to_string(),
        "arm64" | "aarch64" => "aarch64".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_preserves_repeated_identity_files_and_proxyjump() {
        let parsed = parse_ssh_g(
            "dev",
            "host dev\nhostname 10.0.0.2\nuser david\nport 2222\nidentityfile a\nidentityfile b\nproxyjump bastion\n",
        )
        .unwrap();
        assert_eq!(parsed.hostname, "10.0.0.2");
        assert_eq!(parsed.port, 2222);
        assert_eq!(parsed.identity_files, vec!["a", "b"]);
        assert_eq!(parsed.proxy_jump.as_deref(), Some("bastion"));
    }

    #[test]
    fn wildcard_and_option_like_aliases_are_rejected() {
        for alias in ["*", "dev?", "!prod", "-oProxyCommand=x", "with space"] {
            assert!(validate_alias(alias).is_err(), "{alias}");
        }
        validate_alias("dev-box.example").unwrap();
    }

    #[test]
    fn known_host_lookup_prefers_host_key_alias_and_configured_files() {
        let root = std::env::temp_dir().join(format!("wta-known-host-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let known_hosts = root.join("known_hosts");
        // This is a syntactically valid public key fixture only; no private
        // material is used or read by the lookup.
        fs::write(
            &known_hosts,
            "stable-host ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILlROjI/8Fs1Zn3f+Z0ejbGicghJ7KGY0KLR4mm3P1uT\n",
        )
        .unwrap();
        let mut effective_options = BTreeMap::new();
        effective_options.insert("hostkeyalias".into(), vec!["stable-host".into()]);
        effective_options.insert(
            "userknownhostsfile".into(),
            vec![known_hosts.to_string_lossy().into_owned()],
        );
        let resolved = ResolvedSshTarget {
            alias: "dev".into(),
            hostname: "new.example".into(),
            user: None,
            port: 22,
            identity_files: Vec::new(),
            proxy_jump: None,
            proxy_command: None,
            effective_options,
        };
        let keygen = which::which("ssh-keygen.exe")
            .or_else(|_| which::which("ssh-keygen"))
            .unwrap();
        let keys = known_host_keys(&resolved, &keygen).unwrap();
        assert!(String::from_utf8(keys).unwrap().contains("stable-host"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn include_enumeration_is_recursive_and_cycle_safe() {
        let root = std::env::temp_dir().join(format!("wta-ssh-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("conf.d")).unwrap();
        fs::write(
            root.join("config"),
            "Include conf.d/*.conf\nHost root wildcard-*\n",
        )
        .unwrap();
        fs::write(
            root.join("conf.d").join("a.conf"),
            "Include ../config\nHost dev prod\nHost !negated\n",
        )
        .unwrap();
        let mut aliases = BTreeSet::new();
        enumerate_config(&root.join("config"), 0, &mut BTreeSet::new(), &mut aliases).unwrap();
        assert_eq!(
            aliases.into_iter().collect::<Vec<_>>(),
            vec!["dev", "prod", "root"]
        );
    }
}
