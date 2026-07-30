use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneContext {
    /// Terminal session GUID for the agent pane itself.
    pub pane_id: Option<String>,
    /// Stable WT tab id. This is the canonical workspace id in the UI model.
    pub tab_id: Option<String>,
    pub window_id: Option<String>,
    pub cwd: Option<String>,
    /// Terminal session GUID that owns the current surface.
    #[serde(default)]
    pub terminal_session_id: Option<String>,
    /// Stable workspace id. Kept separate from `tab_id` on the wire so future
    /// workspace persistence does not need to overload a UI implementation
    /// detail; today it is normally the same StableId.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Pane-local surface id reported by Terminal Protocol.
    #[serde(default)]
    pub surface_id: Option<String>,
    /// Monotonic focus generation supplied by the host. Consumers must ignore
    /// async results created for an older generation.
    #[serde(default)]
    pub focus_generation: Option<u64>,
    pub source_pane_id: Option<String>,
}

impl PaneContext {
    pub fn effective_source_pane_id(&self) -> Option<&str> {
        self.source_pane_id.as_deref().or(self.pane_id.as_deref())
    }

    /// Stable chat/session scope key. Surface is the default scope; old hosts
    /// that do not advertise one remain safely isolated at workspace level.
    pub fn scope_key(&self) -> Option<String> {
        let workspace = self.workspace_id.as_deref().or(self.tab_id.as_deref())?;
        match self
            .surface_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
        {
            Some(surface) => Some(format!("{workspace}::surface::{surface}")),
            None => Some(workspace.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(source: Option<&str>, pane: Option<&str>) -> PaneContext {
        PaneContext {
            pane_id: pane.map(String::from),
            source_pane_id: source.map(String::from),
            ..Default::default()
        }
    }

    /// `effective_source_pane_id` prefers `source_pane_id` (the pane that
    /// actually produced the failing command) and only falls back to
    /// `pane_id` (the agent pane) when no source is recorded. Autofix routing
    /// depends on this precedence — a regression would land fixes in the wrong
    /// pane.
    #[test]
    fn effective_source_prefers_source_then_falls_back_to_pane() {
        // Both present → source wins.
        assert_eq!(
            ctx(Some("src"), Some("pane")).effective_source_pane_id(),
            Some("src")
        );
        // Only pane present → fall back to pane.
        assert_eq!(
            ctx(None, Some("pane")).effective_source_pane_id(),
            Some("pane")
        );
        // Only source present → source.
        assert_eq!(
            ctx(Some("src"), None).effective_source_pane_id(),
            Some("src")
        );
        // Neither → None (must not invent a target pane).
        assert_eq!(ctx(None, None).effective_source_pane_id(), None);
    }

    #[test]
    fn scope_key_prefers_surface_and_falls_back_to_workspace() {
        let surface = PaneContext {
            workspace_id: Some("workspace-a".into()),
            tab_id: Some("legacy-tab".into()),
            surface_id: Some("7".into()),
            ..Default::default()
        };
        assert_eq!(
            surface.scope_key().as_deref(),
            Some("workspace-a::surface::7")
        );

        let legacy = PaneContext {
            tab_id: Some("tab-a".into()),
            ..Default::default()
        };
        assert_eq!(legacy.scope_key().as_deref(), Some("tab-a"));
        assert_eq!(PaneContext::default().scope_key(), None);
    }
}
