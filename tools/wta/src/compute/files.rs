//! Capability-scoped remote file explorer primitives.
//!
//! File transfer remains owned by `transfer`; this module supplies metadata and
//! small text previews for the native explorer. Every request is relative to an
//! explicitly selected root and canonicalized before access, so `..` and
//! symlink escapes cannot cross the workspace boundary.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Serialize;
use serde_json::{json, Value};

const DEFAULT_PAGE_SIZE: usize = 200;
const MAX_PAGE_SIZE: usize = 500;
const DEFAULT_PREVIEW_BYTES: usize = 256 * 1024;
const MAX_PREVIEW_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteFileKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone)]
struct AuthorizedRoot {
    workspace_id: String,
    binding_id: Option<String>,
    canonical_path: PathBuf,
    readable: bool,
    writable: bool,
    deletable: bool,
}

static AUTHORIZED_ROOTS: OnceLock<Mutex<HashMap<String, AuthorizedRoot>>> = OnceLock::new();

fn authorized_roots() -> &'static Mutex<HashMap<String, AuthorizedRoot>> {
    AUTHORIZED_ROOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register one broker-authorized root for this bridge process.
///
/// The absolute path is accepted only on this internal SSH/stdio hop and is
/// never echoed back. All public CLI/UI operations continue with the opaque
/// root ID plus a workspace-relative path.
pub fn open_root(params: &Value) -> Result<Value> {
    let root_id = required_string(params, "root_id")?;
    validate_opaque_id("root_id", &root_id)?;
    let workspace_id = required_string(params, "workspace_id")?;
    validate_opaque_id("workspace_id", &workspace_id)?;
    let binding_id = params
        .get("binding_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if let Some(binding_id) = binding_id.as_deref() {
        validate_opaque_id("binding_id", binding_id)?;
    }
    let path_b64 = required_string(params, "canonical_path_b64")?;
    let requested_path = decode_path(&path_b64, "canonical_path_b64")?;
    let requested_path = if matches!(
        requested_path.to_string_lossy().as_ref(),
        "~" | "$HOME" | "%USERPROFILE%"
    ) {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .context("remote home directory is unavailable")?
    } else {
        requested_path
    };
    let canonical_path = requested_path
        .canonicalize()
        .context("authorized file root does not exist")?;
    if !fs::metadata(&canonical_path)?.is_dir() {
        bail!("authorized file root must be a directory");
    }

    let readable = params
        .get("readable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let writable = params
        .get("writable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let deletable = params
        .get("deletable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !readable || (writable && !readable) || (deletable && !writable) {
        bail!("authorized file root capabilities are invalid");
    }

    let source = required_string(params, "source")?;
    let wide_scope_acknowledged = params
        .get("wide_scope_acknowledged")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if matches!(source.as_str(), "explicit_home" | "admin") && !wide_scope_acknowledged {
        bail!("broad remote file roots require explicit acknowledgement");
    }
    if !matches!(
        source.as_str(),
        "project" | "worktree" | "explicit_home" | "admin"
    ) {
        bail!("authorized file root source is unsupported");
    }

    let mut roots = authorized_roots()
        .lock()
        .map_err(|_| anyhow::anyhow!("remote file root registry is poisoned"))?;
    roots.insert(
        root_id.clone(),
        AuthorizedRoot {
            workspace_id,
            binding_id,
            canonical_path,
            readable,
            writable,
            deletable,
        },
    );
    Ok(json!({
        "root_id": root_id,
        "registered": true,
        "readable": readable,
        "writable": writable,
        "deletable": deletable,
    }))
}

pub fn close_root(params: &Value) -> Result<Value> {
    let root_id = required_string(params, "root_id")?;
    let removed = authorized_roots()
        .lock()
        .map_err(|_| anyhow::anyhow!("remote file root registry is poisoned"))?
        .remove(&root_id)
        .is_some();
    Ok(json!({"root_id": root_id, "closed": removed}))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteFileEntry {
    pub name: String,
    pub path: String,
    pub path_b64: String,
    pub kind: RemoteFileKind,
    pub size: u64,
    pub modified_at_ms: Option<u64>,
    pub readonly: bool,
    pub hidden: bool,
}

pub fn list_directory(params: &Value) -> Result<Value> {
    let scope = ScopedPath::from_params(params, false, FilePermission::Read)?;
    let metadata = fs::metadata(&scope.resolved)
        .with_context(|| format!("failed to inspect {}", scope.resolved.display()))?;
    if !metadata.is_dir() {
        bail!("file.list_directory target is not a directory");
    }

    let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE);

    let parent_relative = scope.relative.clone();
    let mut entries = fs::read_dir(&scope.resolved)
        .with_context(|| format!("failed to list {}", scope.resolved.display()))?
        .map(|entry| entry.and_then(|entry| entry_from_dir_entry(entry, &parent_relative)))
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by(entry_order);
    let total = entries.len();
    let page = entries
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();

    Ok(json!({
        "root_id": scope.root_id,
        "path_b64": scope.path_b64,
        "entries": page,
        "offset": offset,
        "limit": limit,
        "total": total,
        "has_more": offset.saturating_add(limit) < total,
    }))
}

pub fn stat(params: &Value) -> Result<Value> {
    let scope = ScopedPath::from_params(params, false, FilePermission::Read)?;
    let metadata = fs::symlink_metadata(&scope.resolved)
        .with_context(|| format!("failed to inspect {}", scope.resolved.display()))?;
    let name = scope
        .resolved
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(".");
    Ok(serde_json::to_value(entry_from_metadata(
        name,
        &scope.relative,
        &metadata,
    ))?)
}

pub fn read_text(params: &Value) -> Result<Value> {
    let scope = ScopedPath::from_params(params, false, FilePermission::Read)?;
    let metadata = fs::metadata(&scope.resolved)
        .with_context(|| format!("failed to inspect {}", scope.resolved.display()))?;
    if !metadata.is_file() {
        bail!("file.read_text target is not a regular file");
    }
    let max_bytes = params
        .get("max_bytes")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(DEFAULT_PREVIEW_BYTES)
        .clamp(1, MAX_PREVIEW_BYTES);
    let bytes = fs::read(&scope.resolved)
        .with_context(|| format!("failed to read {}", scope.resolved.display()))?;
    let truncated = bytes.len() > max_bytes;
    let preview = &bytes[..bytes.len().min(max_bytes)];
    if preview.contains(&0) {
        bail!("file.read_text refuses binary content");
    }
    let text = std::str::from_utf8(preview).context("file.read_text requires UTF-8 content")?;
    Ok(json!({
        "root_id": scope.root_id,
        "path_b64": scope.path_b64,
        "text": text,
        "size": metadata.len(),
        "truncated": truncated,
    }))
}

pub fn prepare_download(params: &Value) -> Result<Value> {
    let scope = ScopedPath::from_params(params, false, FilePermission::Read)?;
    let transfer_id = required_string(params, "transfer_id")?;
    let metadata = fs::symlink_metadata(&scope.resolved)
        .with_context(|| format!("failed to inspect {}", scope.resolved.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("file.prepare_download requires a regular non-symlink file");
    }
    let download = super::transfer::prepare_download_scoped(&transfer_id, &scope.resolved)?;
    // The node-side canonical source/snapshot paths are implementation
    // details. They must not cross the scoped file RPC boundary.
    Ok(json!({
        "schema_version": download.schema_version,
        "transfer_id": download.transfer_id,
        "display_name": download.display_name,
        "expected_size": download.expected_size,
        "expected_sha256": download.expected_sha256,
        "state": download.state,
    }))
}

pub fn create_directory(params: &Value) -> Result<Value> {
    require_destructive(params)?;
    let scope = ScopedPath::from_params(params, true, FilePermission::Write)?;
    if scope.relative.as_os_str().is_empty() {
        bail!("file.create_directory cannot replace the workspace root");
    }
    if params
        .get("recursive")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        fs::create_dir_all(&scope.resolved)?;
    } else {
        fs::create_dir(&scope.resolved)?;
    }
    Ok(json!({"ok": true, "path_b64": scope.path_b64}))
}

pub fn rename(params: &Value) -> Result<Value> {
    require_destructive(params)?;
    let source =
        ScopedPath::from_params_with_key(params, "source_b64", false, FilePermission::Write)?;
    let destination =
        ScopedPath::from_params_with_key(params, "destination_b64", true, FilePermission::Write)?;
    if source.relative.as_os_str().is_empty() || destination.relative.as_os_str().is_empty() {
        bail!("file.rename cannot move the workspace root");
    }
    if destination.resolved.exists()
        && !params
            .get("overwrite")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        bail!("file.rename destination already exists");
    }
    if let Some(parent) = destination.resolved.parent() {
        ensure_existing_within(&destination.root, parent)?;
    }
    fs::rename(&source.resolved, &destination.resolved)?;
    Ok(json!({
        "ok": true,
        "source_b64": source.path_b64,
        "destination_b64": destination.path_b64,
    }))
}

pub fn remove(params: &Value) -> Result<Value> {
    require_destructive(params)?;
    let scope = ScopedPath::from_params(params, false, FilePermission::Delete)?;
    if scope.relative.as_os_str().is_empty() {
        bail!("file.remove cannot delete the workspace root");
    }
    let metadata = fs::symlink_metadata(&scope.resolved)?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        fs::remove_file(&scope.resolved)?;
    } else if metadata.is_dir() {
        if params
            .get("recursive")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            fs::remove_dir_all(&scope.resolved)?;
        } else {
            fs::remove_dir(&scope.resolved)?;
        }
    } else {
        bail!("file.remove target type is unsupported");
    }
    Ok(json!({"ok": true, "path_b64": scope.path_b64}))
}

#[derive(Debug)]
struct ScopedPath {
    root: PathBuf,
    resolved: PathBuf,
    relative: PathBuf,
    root_id: String,
    path_b64: String,
}

#[derive(Debug, Clone, Copy)]
enum FilePermission {
    Read,
    Write,
    Delete,
}

impl ScopedPath {
    fn from_params(
        params: &Value,
        allow_missing_leaf: bool,
        permission: FilePermission,
    ) -> Result<Self> {
        Self::from_params_with_key(params, "path_b64", allow_missing_leaf, permission)
    }

    fn from_params_with_key(
        params: &Value,
        path_key: &str,
        allow_missing_leaf: bool,
        permission: FilePermission,
    ) -> Result<Self> {
        let root_id = required_string(params, "root_id")?;
        let workspace_id = required_string(params, "workspace_id")?;
        let requested_binding = params
            .get("binding_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let authorized = authorized_roots()
            .lock()
            .map_err(|_| anyhow::anyhow!("remote file root registry is poisoned"))?
            .get(&root_id)
            .cloned()
            .with_context(|| format!("remote file root is not authorized: {root_id}"))?;
        if authorized.workspace_id != workspace_id
            || authorized.binding_id.as_deref() != requested_binding
        {
            bail!("remote file root is not authorized for this workspace or binding");
        }
        let permitted = match permission {
            FilePermission::Read => authorized.readable,
            FilePermission::Write => authorized.writable,
            FilePermission::Delete => authorized.deletable,
        };
        if !permitted {
            bail!("remote file root does not grant the requested capability");
        }
        let path_b64 = params
            .get(path_key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let relative = decode_path(&path_b64, path_key)?;
        let root = authorized.canonical_path;
        validate_relative(&relative)?;
        let candidate = root.join(&relative);
        let resolved = if allow_missing_leaf && !candidate.exists() {
            let parent = candidate
                .parent()
                .context("file operation target has no parent")?;
            ensure_existing_within(&root, parent)?;
            candidate
        } else {
            ensure_existing_within(&root, &candidate)?
        };
        Ok(Self {
            root,
            resolved,
            relative,
            root_id,
            path_b64,
        })
    }
}

fn required_string(params: &Value, key: &str) -> Result<String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .with_context(|| format!("file operation requires {key}"))
}

fn decode_path(encoded: &str, field: &str) -> Result<PathBuf> {
    if encoded.is_empty() {
        return Ok(PathBuf::new());
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .with_context(|| format!("{field} is not valid base64url"))?;
    let value = String::from_utf8(bytes).with_context(|| format!("{field} is not UTF-8"))?;
    Ok(PathBuf::from(value))
}

fn validate_relative(path: &Path) -> Result<()> {
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("file path must remain relative to the workspace root")
            }
        }
    }
    Ok(())
}

fn validate_opaque_id(field: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':'))
    {
        bail!("{field} is not a valid opaque identifier");
    }
    Ok(())
}

fn ensure_existing_within(root: &Path, candidate: &Path) -> Result<PathBuf> {
    let resolved = candidate
        .canonicalize()
        .with_context(|| format!("path does not exist: {}", candidate.display()))?;
    if !resolved.starts_with(root) {
        bail!("file path escapes the workspace root");
    }
    Ok(resolved)
}

fn require_destructive(params: &Value) -> Result<()> {
    if !params
        .get("allow_destructive")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        bail!("destructive file operation requires allow_destructive=true");
    }
    Ok(())
}

fn entry_from_dir_entry(
    entry: fs::DirEntry,
    parent_relative: &Path,
) -> std::io::Result<RemoteFileEntry> {
    let metadata = fs::symlink_metadata(entry.path())?;
    let name = entry.file_name().to_string_lossy().into_owned();
    Ok(entry_from_metadata(
        &name,
        &parent_relative.join(&name),
        &metadata,
    ))
}

fn entry_from_metadata(name: &str, relative: &Path, metadata: &fs::Metadata) -> RemoteFileEntry {
    let file_type = metadata.file_type();
    let kind = if file_type.is_symlink() {
        RemoteFileKind::Symlink
    } else if metadata.is_dir() {
        RemoteFileKind::Directory
    } else if metadata.is_file() {
        RemoteFileKind::File
    } else {
        RemoteFileKind::Other
    };
    RemoteFileEntry {
        name: name.to_string(),
        path: relative.to_string_lossy().into_owned(),
        path_b64: URL_SAFE_NO_PAD.encode(relative.to_string_lossy().as_bytes()),
        kind,
        size: metadata.len(),
        modified_at_ms: metadata.modified().ok().and_then(system_time_ms),
        readonly: metadata.permissions().readonly(),
        hidden: name.starts_with('.'),
    }
}

fn entry_order(left: &RemoteFileEntry, right: &RemoteFileEntry) -> Ordering {
    let left_dir = left.kind == RemoteFileKind::Directory;
    let right_dir = right.kind == RemoteFileKind::Directory;
    right_dir
        .cmp(&left_dir)
        .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
        .then_with(|| left.name.cmp(&right.name))
}

fn system_time_ms(value: SystemTime) -> Option<u64> {
    value
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> std::io::Result<Self> {
            let path =
                std::env::temp_dir().join(format!("wta-files-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn encoded(path: &Path) -> String {
        URL_SAFE_NO_PAD.encode(path.to_string_lossy().as_bytes())
    }

    fn params_with_capabilities(
        root: &Path,
        relative: &str,
        writable: bool,
        deletable: bool,
    ) -> Value {
        let root_id = format!("root-{}", uuid::Uuid::new_v4());
        open_root(&json!({
            "root_id": root_id,
            "workspace_id": "workspace-test",
            "canonical_path_b64": encoded(root),
            "readable": true,
            "writable": writable,
            "deletable": deletable,
            "source": "project",
        }))
        .unwrap();
        json!({
            "root_id": root_id,
            "workspace_id": "workspace-test",
            "path_b64": URL_SAFE_NO_PAD.encode(relative.as_bytes()),
        })
    }

    fn params(root: &Path, relative: &str) -> Value {
        params_with_capabilities(root, relative, false, false)
    }

    #[test]
    fn listing_is_sorted_directory_first_and_paginated() {
        let temp = TestDir::new().unwrap();
        fs::write(temp.path().join("z.txt"), "z").unwrap();
        fs::write(temp.path().join("a.txt"), "a").unwrap();
        fs::create_dir(temp.path().join("folder")).unwrap();
        let mut request = params(temp.path(), "");
        request["limit"] = json!(2);
        let value = list_directory(&request).unwrap();
        assert_eq!(value["entries"][0]["name"], "folder");
        assert_eq!(value["entries"][1]["name"], "a.txt");
        assert_eq!(value["has_more"], true);
    }

    #[test]
    fn nested_listing_returns_workspace_relative_paths() {
        let temp = TestDir::new().unwrap();
        fs::create_dir(temp.path().join("nested")).unwrap();
        fs::write(temp.path().join("nested").join("child.txt"), "child").unwrap();
        let value = list_directory(&params(temp.path(), "nested")).unwrap();
        let encoded_path = value["entries"][0]["path_b64"].as_str().unwrap();
        let decoded = URL_SAFE_NO_PAD.decode(encoded_path).unwrap();
        assert_eq!(
            PathBuf::from(String::from_utf8(decoded).unwrap()),
            PathBuf::from("nested").join("child.txt")
        );
    }

    #[test]
    fn traversal_and_absolute_paths_are_rejected() {
        let temp = TestDir::new().unwrap();
        assert!(stat(&params(temp.path(), "../secret")).is_err());
        let absolute = if cfg!(windows) { r"C:\Windows" } else { "/etc" };
        assert!(stat(&params(temp.path(), absolute)).is_err());
    }

    #[test]
    fn text_preview_is_bounded_and_binary_refused() {
        let temp = TestDir::new().unwrap();
        fs::write(temp.path().join("text.txt"), "abcdef").unwrap();
        fs::write(temp.path().join("binary.bin"), [1, 0, 2]).unwrap();
        let mut request = params(temp.path(), "text.txt");
        request["max_bytes"] = json!(3);
        let value = read_text(&request).unwrap();
        assert_eq!(value["text"], "abc");
        assert_eq!(value["truncated"], true);
        assert!(read_text(&params(temp.path(), "binary.bin")).is_err());
    }

    #[test]
    fn scoped_download_manifest_never_exposes_canonical_paths() {
        let temp = TestDir::new().unwrap();
        fs::write(temp.path().join("download.txt"), "content").unwrap();
        let transfer_id = format!("transfer-{}", uuid::Uuid::new_v4());
        let mut request = params(temp.path(), "download.txt");
        request["transfer_id"] = json!(transfer_id);
        let value = prepare_download(&request).unwrap();
        assert_eq!(value["display_name"], "download.txt");
        assert!(value.get("source_path").is_none());
        assert!(value.get("snapshot_path").is_none());
        super::super::transfer::abort_upload(&transfer_id).unwrap();
    }

    #[test]
    fn destructive_operations_are_explicit() {
        let temp = TestDir::new().unwrap();
        let request = params_with_capabilities(temp.path(), "created", true, true);
        assert!(create_directory(&request).is_err());
        let mut approved = request;
        approved["allow_destructive"] = json!(true);
        create_directory(&approved).unwrap();
        assert!(temp.path().join("created").is_dir());

        assert!(remove(&params(temp.path(), "created")).is_err());
        let mut removal = params_with_capabilities(temp.path(), "created", true, true);
        removal["allow_destructive"] = json!(true);
        remove(&removal).unwrap();
        assert!(!temp.path().join("created").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;
        let root = TestDir::new().unwrap();
        let outside = TestDir::new().unwrap();
        fs::write(outside.path().join("secret"), "no").unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();
        assert!(stat(&params(root.path(), "escape/secret")).is_err());
    }

    #[test]
    fn workspace_and_root_identity_are_fail_closed() {
        let temp = TestDir::new().unwrap();
        fs::write(temp.path().join("file.txt"), "secret").unwrap();
        let request = params(temp.path(), "file.txt");

        let mut other_workspace = request.clone();
        other_workspace["workspace_id"] = json!("workspace-other");
        assert!(read_text(&other_workspace).is_err());

        let mut forged_root = request.clone();
        forged_root["root_id"] = json!("root-forged");
        assert!(read_text(&forged_root).is_err());

        close_root(&json!({"root_id": request["root_id"]})).unwrap();
        assert!(read_text(&request).is_err());
    }

    #[test]
    fn read_only_root_rejects_all_mutations() {
        let temp = TestDir::new().unwrap();
        fs::write(temp.path().join("source.txt"), "content").unwrap();
        let mut mkdir = params(temp.path(), "new");
        mkdir["allow_destructive"] = json!(true);
        assert!(create_directory(&mkdir).is_err());

        let mut rename = params(temp.path(), "");
        rename["source_b64"] = json!(URL_SAFE_NO_PAD.encode("source.txt"));
        rename["destination_b64"] = json!(URL_SAFE_NO_PAD.encode("renamed.txt"));
        rename["allow_destructive"] = json!(true);
        assert!(super::rename(&rename).is_err());

        let mut remove_request = params(temp.path(), "source.txt");
        remove_request["allow_destructive"] = json!(true);
        assert!(remove(&remove_request).is_err());
    }

    #[test]
    fn explicit_home_source_requires_wide_scope_acknowledgement() {
        let temp = TestDir::new().unwrap();
        assert!(open_root(&json!({
            "root_id": format!("root-{}", uuid::Uuid::new_v4()),
            "workspace_id": "workspace-test",
            "canonical_path_b64": encoded(temp.path()),
            "readable": true,
            "writable": false,
            "deletable": false,
            "source": "explicit_home",
        }))
        .is_err());
    }
}
