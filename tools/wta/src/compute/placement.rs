//! Deterministic, explainable placement and sticky target selection.

use std::collections::{BTreeMap, BTreeSet};

use uuid::Uuid;

use super::model::*;
use super::store::{now_ms, ComputeStore};

pub fn decide(
    store: &ComputeStore,
    request: &PlacementRequest,
) -> anyhow::Result<PlacementDecision> {
    let targets = store.list_targets()?;
    let leases = store.list_leases()?;
    let excluded = request
        .excluded_target_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut candidates = targets
        .iter()
        .map(|target| {
            let mut reasons = Vec::new();
            if target.disabled {
                reasons.push("target_disabled".to_string());
            }
            if !matches!(
                target.health,
                TargetHealth::Healthy | TargetHealth::Degraded
            ) {
                reasons.push(format!("health_{:?}", target.health).to_ascii_lowercase());
            }
            if excluded.contains(target.id.as_str()) {
                reasons.push("request_excluded".to_string());
            }
            if target.trust_tier == TrustTier::Production && !request.production_targets_allowed {
                reasons.push("production_auto_placement_forbidden".to_string());
            }
            if !target.trust_tier.permits(request.required_trust_tier) {
                reasons.push("trust_tier_not_permitted".to_string());
            }
            if request
                .requirements
                .os
                .as_deref()
                .is_some_and(|os| !target.os.eq_ignore_ascii_case(os))
            {
                reasons.push("os_mismatch".to_string());
            }
            if request
                .requirements
                .arch
                .as_deref()
                .is_some_and(|arch| !target.arch.eq_ignore_ascii_case(arch))
            {
                reasons.push("arch_mismatch".to_string());
            }
            if target.memory_bytes < request.requirements.minimum_memory_bytes {
                reasons.push("insufficient_memory".to_string());
            }
            for capability in &request.requirements.capabilities {
                if !target
                    .capabilities
                    .iter()
                    .any(|value| value.eq_ignore_ascii_case(capability))
                {
                    reasons.push(format!("missing_capability:{capability}"));
                }
            }
            for (tool, version) in &request.requirements.toolchains {
                match target.toolchains.get(tool) {
                    Some(actual) if version.is_empty() || actual == version => {}
                    Some(_) => reasons.push(format!("toolchain_version_mismatch:{tool}")),
                    None => reasons.push(format!("missing_toolchain:{tool}")),
                }
            }
            if !target.project_allowlist.is_empty()
                && !request.requirements.project_identity.is_empty()
                && !target
                    .project_allowlist
                    .iter()
                    .any(|project| project == &request.requirements.project_identity)
            {
                reasons.push("project_not_allowed".to_string());
            }

            let slot_kind = match request.workload.slot_class() {
                SlotClass::Agent => LeaseKind::AgentSlot,
                SlotClass::Build => LeaseKind::BuildSlot,
            };
            let capacity = match request.workload.slot_class() {
                SlotClass::Agent => target.agent_slots,
                SlotClass::Build => target.build_slots,
            };
            let used = leases
                .iter()
                .filter(|lease| {
                    lease.state == LeaseState::Active
                        && lease.target_id.as_deref() == Some(target.id.as_str())
                        && lease.kind == slot_kind
                })
                .count() as u32;
            if used >= capacity {
                reasons.push("slot_capacity_exhausted".to_string());
            }

            let mut score = BTreeMap::new();
            let free_ratio = if capacity == 0 {
                0.0
            } else {
                f64::from(capacity.saturating_sub(used)) / f64::from(capacity)
            };
            score.insert("free_slots".to_string(), free_ratio * 40.0);
            score.insert(
                "health".to_string(),
                if target.health == TargetHealth::Healthy {
                    20.0
                } else {
                    8.0
                },
            );
            score.insert(
                "locality".to_string(),
                if target.provider == ProviderKind::Local {
                    match request.candidate_policy {
                        PlacementPolicy::LocalFirst => 40.0,
                        PlacementPolicy::Balanced => 10.0,
                        PlacementPolicy::CostFirst => 15.0,
                        PlacementPolicy::Performance => 5.0,
                    }
                } else {
                    0.0
                },
            );
            if request.preferred_target_id.as_deref() == Some(target.id.as_str()) {
                score.insert("preferred".to_string(), 100.0);
            }
            if let Some(latency) = target
                .metadata
                .get("latency_ms")
                .and_then(serde_json::Value::as_f64)
            {
                score.insert("latency".to_string(), (30.0 - latency.min(30.0)).max(0.0));
            }
            if request.candidate_policy == PlacementPolicy::CostFirst {
                let hourly = target
                    .cost_policy
                    .get("hourly_usd")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(if target.provider == ProviderKind::Local {
                        0.0
                    } else {
                        1.0
                    });
                score.insert("cost".to_string(), (30.0 - hourly.min(30.0)).max(0.0));
            }
            let total = if reasons.is_empty() {
                score.values().sum()
            } else {
                0.0
            };
            PlacementCandidate {
                target_id: target.id.clone(),
                eligible: reasons.is_empty(),
                exclusion_reasons: reasons,
                score_components: score,
                total_score: total,
            }
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| {
        right
            .eligible
            .cmp(&left.eligible)
            .then_with(|| {
                right
                    .total_score
                    .partial_cmp(&left.total_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| left.target_id.cmp(&right.target_id))
    });
    let selected_target_id = candidates
        .iter()
        .find(|candidate| candidate.eligible)
        .map(|candidate| candidate.target_id.clone());
    Ok(PlacementDecision {
        schema_version: COMPUTE_SCHEMA_VERSION,
        decision_id: Uuid::new_v4().to_string(),
        selected_target_id,
        candidates,
        policy_version: PLACEMENT_POLICY_VERSION.to_string(),
        created_at_ms: now_ms(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compute::store::ComputeStore;
    use serde_json::Value;

    fn target(id: &str, provider: ProviderKind, trust: TrustTier) -> ComputeTarget {
        ComputeTarget {
            schema_version: COMPUTE_SCHEMA_VERSION,
            id: id.into(),
            display_name: id.into(),
            provider,
            endpoint: TargetEndpoint::default(),
            os: "windows".into(),
            arch: "x86_64".into(),
            capabilities: vec!["codex".into()],
            toolchains: BTreeMap::new(),
            trust_tier: trust,
            project_allowlist: Vec::new(),
            agent_slots: 2,
            build_slots: 2,
            memory_bytes: 16,
            cost_policy: Value::Null,
            power_policy: Value::Null,
            health: TargetHealth::Healthy,
            last_probe_at_ms: None,
            disabled: false,
            metadata: Value::Null,
        }
    }

    fn request() -> PlacementRequest {
        PlacementRequest {
            schema_version: COMPUTE_SCHEMA_VERSION,
            request_id: "request".into(),
            workspace_id: "workspace".into(),
            workload: WorkloadClass::InteractiveAgent,
            requirements: PlacementRequirements {
                capabilities: vec!["codex".into()],
                ..Default::default()
            },
            candidate_policy: PlacementPolicy::LocalFirst,
            preferred_target_id: None,
            excluded_target_ids: Vec::new(),
            production_targets_allowed: false,
            required_trust_tier: TrustTier::Development,
        }
    }

    #[test]
    fn deterministic_and_excludes_production() {
        let root = std::env::temp_dir().join(format!("wta-placement-{}", Uuid::new_v4()));
        let store = ComputeStore::at(root).unwrap();
        store
            .upsert_target(
                "test",
                target("local", ProviderKind::Local, TrustTier::Personal),
            )
            .unwrap();
        store
            .upsert_target(
                "test",
                target("prod", ProviderKind::Ssh, TrustTier::Production),
            )
            .unwrap_err(); // SSH requires an alias; prove validation first.
        let mut prod = target("prod", ProviderKind::Ssh, TrustTier::Production);
        prod.endpoint.ssh_alias = Some("prod".into());
        store.upsert_target("test", prod).unwrap();
        let left = decide(&store, &request()).unwrap();
        let right = decide(&store, &request()).unwrap();
        assert_eq!(left.selected_target_id.as_deref(), Some("local"));
        assert_eq!(
            left.candidates
                .iter()
                .map(|candidate| (&candidate.target_id, candidate.eligible))
                .collect::<Vec<_>>(),
            right
                .candidates
                .iter()
                .map(|candidate| (&candidate.target_id, candidate.eligible))
                .collect::<Vec<_>>()
        );
        let prod = left
            .candidates
            .iter()
            .find(|candidate| candidate.target_id == "prod")
            .unwrap();
        assert!(prod
            .exclusion_reasons
            .contains(&"production_auto_placement_forbidden".to_string()));
    }
}
