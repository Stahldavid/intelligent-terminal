//! Shared SSH transport policy and reconnect state machine.
//!
//! The UI, CLI and future background supervisor consume this deterministic
//! core so retry timing and keepalive injection cannot drift between callers.

use super::model::ReconnectPolicy;
use super::ssh::ResolvedSshTarget;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectController {
    policy: ReconnectPolicy,
    attempt: u32,
}

impl ReconnectController {
    pub fn new(policy: ReconnectPolicy, attempt: u32) -> Self {
        Self { policy, attempt }
    }

    pub fn next_delay_seconds(&self) -> u64 {
        let index = usize::try_from(self.attempt).unwrap_or(usize::MAX);
        self.policy
            .delays_seconds
            .get(index)
            .copied()
            .or_else(|| self.policy.delays_seconds.last().copied())
            .unwrap_or(self.policy.ceiling_seconds)
            .min(self.policy.ceiling_seconds)
    }

    pub fn record_failure(&mut self) -> u64 {
        let delay = self.next_delay_seconds();
        self.attempt = self.attempt.saturating_add(1);
        delay
    }

    pub fn record_connected(&mut self) {
        self.attempt = 0;
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }
}

pub fn default_keepalive_args(resolved: &ResolvedSshTarget) -> Vec<String> {
    let has_interval = resolved
        .effective_options
        .contains_key("serveraliveinterval");
    let has_count = resolved
        .effective_options
        .contains_key("serveralivecountmax");
    let mut args = Vec::new();
    if !has_interval {
        args.extend(["-o".to_string(), "ServerAliveInterval=20".to_string()]);
    }
    if !has_count {
        args.extend(["-o".to_string(), "ServerAliveCountMax=2".to_string()]);
    }
    args
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn resolved() -> ResolvedSshTarget {
        ResolvedSshTarget {
            alias: "dev".into(),
            hostname: "dev.example".into(),
            user: None,
            port: 22,
            identity_files: Vec::new(),
            proxy_jump: None,
            proxy_command: None,
            effective_options: BTreeMap::new(),
        }
    }

    #[test]
    fn reconnect_backoff_is_deterministic_and_capped() {
        let mut controller = ReconnectController::new(ReconnectPolicy::default(), 0);
        let delays = (0..8)
            .map(|_| controller.record_failure())
            .collect::<Vec<_>>();
        assert_eq!(delays, vec![3, 6, 12, 24, 48, 60, 60, 60]);
        controller.record_connected();
        assert_eq!(controller.attempt(), 0);
        assert_eq!(controller.next_delay_seconds(), 3);
    }

    #[test]
    fn keepalive_is_only_injected_when_config_does_not_define_it() {
        let mut target = resolved();
        assert_eq!(
            default_keepalive_args(&target),
            vec![
                "-o",
                "ServerAliveInterval=20",
                "-o",
                "ServerAliveCountMax=2"
            ]
        );
        target
            .effective_options
            .insert("serveraliveinterval".into(), vec!["9".into()]);
        assert_eq!(
            default_keepalive_args(&target),
            vec!["-o", "ServerAliveCountMax=2"]
        );
    }
}
