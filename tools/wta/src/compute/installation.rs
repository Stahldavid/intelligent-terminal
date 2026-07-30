//! Canonical remote `wta-node` installation contract.
//!
//! Bootstrap, ACP spawn and diagnostics must all resolve the same active
//! executable. Keeping that layout here prevents a successfully installed
//! version from becoming unreachable through a second hard-coded path.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::model::ComputeTarget;
use super::store::now_ms;

pub const NODE_INSTALLATION_SCHEMA_VERSION: u16 = 1;
const NODE_INSTALLATION_METADATA_KEY: &str = "node_installation";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteNodeLayout {
    pub root: String,
    pub version_dir: String,
    pub version_path: String,
    pub active_dir: String,
    pub active_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeInstallation {
    pub schema_version: u16,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub version_path: String,
    pub active_path: String,
    pub state_root: String,
    pub sha256: String,
    pub activated_at_ms: u64,
}

impl NodeInstallation {
    pub fn new(
        target: &ComputeTarget,
        layout: &RemoteNodeLayout,
        state_root: String,
        sha256: String,
    ) -> Self {
        Self {
            schema_version: NODE_INSTALLATION_SCHEMA_VERSION,
            version: env!("CARGO_PKG_VERSION").to_string(),
            os: target.os.clone(),
            arch: target.arch.clone(),
            version_path: layout.version_path.clone(),
            active_path: layout.active_path.clone(),
            state_root,
            sha256,
            activated_at_ms: now_ms(),
        }
    }

    pub fn validate_for(&self, target: &ComputeTarget) -> Result<()> {
        if self.schema_version != NODE_INSTALLATION_SCHEMA_VERSION {
            bail!(
                "unsupported node installation schema {}",
                self.schema_version
            );
        }
        if self.version != env!("CARGO_PKG_VERSION") {
            bail!(
                "target '{}' has wta-node {}, but this build requires {}",
                target.id,
                self.version,
                env!("CARGO_PKG_VERSION")
            );
        }
        if !self.os.eq_ignore_ascii_case(&target.os) || self.arch != target.arch {
            bail!(
                "target '{}' node installation platform {}/{} does not match target {}/{}",
                target.id,
                self.os,
                self.arch,
                target.os,
                target.arch
            );
        }
        validate_remote_path("version_path", &self.version_path)?;
        validate_remote_path("active_path", &self.active_path)?;
        if self.state_root.trim().is_empty() || self.state_root.contains('\0') {
            bail!("target '{}' node state_root is invalid", target.id);
        }
        if self.sha256.len() != 64 || !self.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!(
                "target '{}' node installation has invalid SHA-256",
                target.id
            );
        }
        Ok(())
    }
}

pub fn layout_for(target: &ComputeTarget) -> Result<RemoteNodeLayout> {
    let version = env!("CARGO_PKG_VERSION");
    let (root, executable) = if target.os.eq_ignore_ascii_case("windows") {
        (".intelligent-terminal-node", "wta-node.exe")
    } else if target.os.eq_ignore_ascii_case("linux") {
        (".local/state/intelligent-terminal-node", "wta-node")
    } else {
        bail!(
            "wta-node installation is unsupported on target OS '{}'",
            target.os
        );
    };
    let version_dir = format!("{root}/versions/{version}");
    let active_dir = format!("{root}/current");
    Ok(RemoteNodeLayout {
        root: root.to_string(),
        version_path: format!("{version_dir}/{executable}"),
        version_dir,
        active_path: format!("{active_dir}/{executable}"),
        active_dir,
    })
}

pub fn record(target: &mut ComputeTarget, installation: &NodeInstallation) -> Result<()> {
    installation.validate_for(target)?;
    if !target.metadata.is_object() {
        target.metadata = Value::Object(Map::new());
    }
    let metadata = target
        .metadata
        .as_object_mut()
        .expect("metadata was replaced with an object");
    metadata.insert(
        NODE_INSTALLATION_METADATA_KEY.to_string(),
        serde_json::to_value(installation)?,
    );
    Ok(())
}

pub fn from_target(target: &ComputeTarget) -> Result<NodeInstallation> {
    let value = target
        .metadata
        .get(NODE_INSTALLATION_METADATA_KEY)
        .with_context(|| {
            format!(
                "compute target '{}' has no active wta-node installation; run `wta compute node bootstrap {}`",
                target.id, target.id
            )
        })?;
    let installation: NodeInstallation =
        serde_json::from_value(value.clone()).with_context(|| {
            format!(
                "target '{}' node installation metadata is invalid",
                target.id
            )
        })?;
    installation.validate_for(target)?;
    Ok(installation)
}

fn validate_remote_path(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.starts_with('/')
        || value.contains('\\')
        || value.split('/').any(|part| part.is_empty() || part == "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'))
    {
        bail!("{label} is not a safe home-relative remote path");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::Value;

    use super::*;
    use crate::compute::model::{
        ProviderKind, TargetEndpoint, TargetHealth, TrustTier, COMPUTE_SCHEMA_VERSION,
    };

    fn linux_target() -> ComputeTarget {
        ComputeTarget {
            schema_version: COMPUTE_SCHEMA_VERSION,
            id: "dev-linux".into(),
            display_name: "dev-linux".into(),
            provider: ProviderKind::Ssh,
            endpoint: TargetEndpoint {
                ssh_alias: Some("dev-linux".into()),
                ..Default::default()
            },
            os: "linux".into(),
            arch: "x86_64".into(),
            capabilities: Vec::new(),
            toolchains: BTreeMap::new(),
            trust_tier: TrustTier::Development,
            project_allowlist: Vec::new(),
            agent_slots: 1,
            build_slots: 1,
            memory_bytes: 0,
            cost_policy: Value::Null,
            power_policy: Value::Null,
            health: TargetHealth::Healthy,
            last_probe_at_ms: None,
            disabled: false,
            metadata: Value::Null,
        }
    }

    #[test]
    fn bootstrap_and_spawn_share_one_canonical_path() {
        let mut target = linux_target();
        let layout = layout_for(&target).unwrap();
        assert_eq!(
            layout.active_path,
            ".local/state/intelligent-terminal-node/current/wta-node"
        );
        let installation = NodeInstallation::new(
            &target,
            &layout,
            "/home/test/.local/state".into(),
            "a".repeat(64),
        );
        record(&mut target, &installation).unwrap();
        assert_eq!(from_target(&target).unwrap(), installation);
    }

    #[test]
    fn stale_or_unsafe_installations_fail_closed() {
        let mut target = linux_target();
        let layout = layout_for(&target).unwrap();
        let mut installation = NodeInstallation::new(
            &target,
            &layout,
            "/home/test/.local/state".into(),
            "a".repeat(64),
        );
        installation.active_path = "../wta-node".into();
        assert!(record(&mut target, &installation).is_err());

        let mut stale = NodeInstallation::new(
            &target,
            &layout,
            "/home/test/.local/state".into(),
            "a".repeat(64),
        );
        stale.version = "0.0.0".into();
        target.metadata = serde_json::json!({NODE_INSTALLATION_METADATA_KEY: stale});
        assert!(from_target(&target).is_err());
    }
}
