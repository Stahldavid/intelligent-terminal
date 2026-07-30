//! Inspectable Git/dirty-tree snapshots with fail-closed path handling.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Component, Path};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::model::*;
use super::store::{now_ms, ComputeStore};

const MAX_UNTRACKED_FILE_BYTES: u64 = 64 * 1024 * 1024;

pub fn create(
    store: &ComputeStore,
    repository: &Path,
    created_by: &str,
    include_ignored: &[String],
) -> Result<SnapshotManifest> {
    let repository = repository
        .canonicalize()
        .with_context(|| format!("repository does not exist: {}", repository.display()))?;
    ensure_git_repository(&repository)?;
    let base_commit = git_optional(&repository, &["rev-parse", "HEAD"])?;
    let patch = git_bytes(
        &repository,
        &["diff", "--binary", "--full-index", "HEAD", "--"],
    )?;
    let tracked_patch_digest = sha256_bytes(&patch);
    let deleted_entries = git_lines(
        &repository,
        &["diff", "--name-only", "--diff-filter=D", "HEAD", "--"],
    )?;
    let mut untracked = git_lines(
        &repository,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?;
    if !include_ignored.is_empty() {
        let ignored = git_lines(
            &repository,
            &[
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "-z",
            ],
        )?;
        for path in ignored {
            if include_ignored
                .iter()
                .any(|pattern| path_matches(pattern, &path))
            {
                untracked.push(path);
            }
        }
    }
    untracked.sort();
    untracked.dedup();

    let snapshot_id = format!("snapshot-{}", Uuid::new_v4());
    let snapshot_root = store.snapshot_path(&snapshot_id);
    let untracked_root = snapshot_root.join("payload").join("untracked");
    fs::create_dir_all(&untracked_root)?;
    fs::write(snapshot_root.join("tracked.patch"), &patch)?;

    let mut untracked_entries = Vec::new();
    let mut excluded_secret_candidates = Vec::new();
    let mut mode_entries = BTreeMap::new();
    for relative in untracked {
        validate_relative_path(&relative)?;
        if looks_like_secret(&relative) {
            excluded_secret_candidates.push(relative);
            continue;
        }
        let source = repository.join(&relative);
        let metadata = fs::symlink_metadata(&source)?;
        if metadata.file_type().is_symlink() {
            excluded_secret_candidates.push(format!("{relative} (symlink)"));
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        if metadata.len() > MAX_UNTRACKED_FILE_BYTES {
            bail!(
                "untracked file exceeds {} bytes: {relative}",
                MAX_UNTRACKED_FILE_BYTES
            );
        }
        let digest = sha256_file(&source)?;
        let destination = untracked_root.join(&relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&source, &destination)?;
        mode_entries.insert(relative.clone(), file_mode(&metadata));
        untracked_entries.push(SnapshotEntry {
            relative_path: relative,
            size_bytes: metadata.len(),
            sha256: digest,
        });
    }
    excluded_secret_candidates.sort();
    let mut overall = Sha256::new();
    overall.update(tracked_patch_digest.as_bytes());
    for entry in &untracked_entries {
        overall.update(entry.relative_path.as_bytes());
        overall.update(entry.sha256.as_bytes());
    }
    for entry in &deleted_entries {
        overall.update(entry.as_bytes());
    }
    let manifest = SnapshotManifest {
        schema_version: COMPUTE_SCHEMA_VERSION,
        snapshot_id,
        format_version: 1,
        repository_identity: repository.to_string_lossy().into_owned(),
        base_commit,
        tracked_patch_digest,
        untracked_entries,
        deleted_entries,
        mode_entries,
        symlink_policy: "exclude".to_string(),
        ignored_includes: include_ignored.to_vec(),
        excluded_secret_candidates,
        overall_digest: hex::encode(overall.finalize()),
        created_by: created_by.to_string(),
        created_at_ms: now_ms(),
    };
    store.save_snapshot(created_by, &manifest)?;
    Ok(manifest)
}

pub fn materialize(store: &ComputeStore, id: &str, destination: &Path) -> Result<()> {
    let manifest = store.get_snapshot(id)?;
    if destination.exists() && destination.read_dir()?.next().is_some() {
        bail!(
            "snapshot destination must be empty: {}",
            destination.display()
        );
    }
    fs::create_dir_all(destination)?;
    let base = manifest
        .base_commit
        .as_deref()
        .context("snapshot has no base commit and cannot be materialized")?;
    run(
        Command::new("git")
            .arg("clone")
            .arg("--no-checkout")
            .arg(&manifest.repository_identity)
            .arg(destination),
        "git clone",
    )?;
    run(
        Command::new("git")
            .current_dir(destination)
            .arg("checkout")
            .arg("--detach")
            .arg(base),
        "git checkout snapshot base",
    )?;
    let snapshot_root = store.snapshot_path(id);
    let patch = snapshot_root.join("tracked.patch");
    if fs::metadata(&patch).map(|value| value.len()).unwrap_or(0) > 0 {
        run(
            Command::new("git")
                .current_dir(destination)
                .arg("apply")
                .arg("--binary")
                .arg("--index")
                .arg(&patch),
            "git apply snapshot patch",
        )?;
    }
    for entry in &manifest.untracked_entries {
        validate_relative_path(&entry.relative_path)?;
        let source = snapshot_root
            .join("payload")
            .join("untracked")
            .join(&entry.relative_path);
        let actual = sha256_file(&source)?;
        if actual != entry.sha256 {
            bail!(
                "snapshot payload digest mismatch for {}",
                entry.relative_path
            );
        }
        let target = destination.join(&entry.relative_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, target)?;
    }
    Ok(())
}

pub fn verify(store: &ComputeStore, id: &str) -> Result<SnapshotManifest> {
    let manifest = store.get_snapshot(id)?;
    let snapshot_root = store.snapshot_path(id);
    let patch = fs::read(snapshot_root.join("tracked.patch"))?;
    if sha256_bytes(&patch) != manifest.tracked_patch_digest {
        bail!("snapshot tracked patch digest mismatch");
    }
    for entry in &manifest.untracked_entries {
        validate_relative_path(&entry.relative_path)?;
        let path = snapshot_root
            .join("payload")
            .join("untracked")
            .join(&entry.relative_path);
        if sha256_file(&path)? != entry.sha256 {
            bail!("snapshot payload digest mismatch: {}", entry.relative_path);
        }
    }
    Ok(manifest)
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn ensure_git_repository(root: &Path) -> Result<()> {
    let value = git_optional(root, &["rev-parse", "--is-inside-work-tree"])?;
    if value.as_deref() != Some("true") {
        bail!("not a Git worktree: {}", root.display());
    }
    Ok(())
}

fn git_optional(root: &Path, args: &[&str]) -> Result<Option<String>> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!value.is_empty()).then_some(value))
}

fn git_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn git_lines(root: &Path, args: &[&str]) -> Result<Vec<String>> {
    let bytes = git_bytes(root, args)?;
    Ok(if bytes.contains(&0) {
        bytes
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect()
    } else {
        String::from_utf8_lossy(&bytes)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect()
    })
}

fn validate_relative_path(value: &str) -> Result<()> {
    let path = Path::new(value);
    let bytes = value.as_bytes();
    // `Path` follows the host platform's syntax. Reject Windows-rooted paths
    // explicitly as well so a snapshot created on Linux cannot smuggle an
    // absolute path that becomes dangerous when materialized on Windows.
    let windows_drive_rooted = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    let windows_rooted = value.starts_with('\\') || value.starts_with("//") || windows_drive_rooted;
    if value.is_empty()
        || path.is_absolute()
        || windows_rooted
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("unsafe snapshot path: {value:?}");
    }
    Ok(())
}

fn looks_like_secret(relative: &str) -> bool {
    let lower = relative.replace('\\', "/").to_ascii_lowercase();
    let name = Path::new(&lower)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    matches!(
        name,
        ".env"
            | ".env.local"
            | ".env.production"
            | "id_rsa"
            | "id_ed25519"
            | "credentials"
            | "credentials.json"
            | "secrets.json"
    ) || name.ends_with(".pem")
        || name.ends_with(".key")
        || lower.contains("/.aws/")
        || lower.contains("/.ssh/")
}

fn path_matches(pattern: &str, value: &str) -> bool {
    if pattern == value {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return value == prefix || value.starts_with(&format!("{prefix}/"));
    }
    false
}

fn file_mode(metadata: &fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return format!("{:o}", metadata.permissions().mode() & 0o777);
    }
    #[cfg(not(unix))]
    {
        if metadata.permissions().readonly() {
            "readonly".to_string()
        } else {
            "normal".to_string()
        }
    }
}

fn run(command: &mut Command, label: &str) -> Result<()> {
    let output = command.stdin(Stdio::null()).output()?;
    if !output.status.success() {
        bail!(
            "{label} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_and_secrets_are_fail_closed() {
        for value in [
            "",
            "../secret",
            "a/../../b",
            "C:\\secret",
            "D:/secret",
            "\\\\server\\share\\secret",
        ] {
            assert!(validate_relative_path(value).is_err(), "{value}");
        }
        for value in [
            ".env",
            "private.key",
            ".ssh/id_ed25519",
            "x/.aws/credentials",
        ] {
            assert!(looks_like_secret(value), "{value}");
        }
        assert!(!looks_like_secret("src/main.rs"));
    }

    #[test]
    fn hash_is_stable() {
        assert_eq!(
            sha256_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
