//! Surface-scoped native browser lifecycle.
//!
//! This module deliberately owns state, profile isolation and the SSH proxy,
//! but not the WebView2 renderer. The native host may only create a controller
//! from a `Ready` contract returned here.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use uuid::Uuid;

use super::model::{
    BrowserSurfaceSession, BrowserSurfaceState, RemoteProxyState, RemoteWorkspaceState,
    COMPUTE_SCHEMA_VERSION,
};
use super::proxy;
use super::store::{now_ms, ComputeStore};

const MAX_URL_BYTES: usize = 8192;
const MAX_HISTORY_ENTRIES: usize = 200;

pub fn open(
    store: &ComputeStore,
    requested_id: Option<&str>,
    remote_workspace_id: &str,
    surface_id: &str,
    initial_url: &str,
    persistent: bool,
    allow_production: bool,
) -> Result<BrowserSurfaceSession> {
    let url = validate_navigation_url(initial_url)?;
    let workspace = store.get_remote_workspace(remote_workspace_id)?;
    if workspace.state != RemoteWorkspaceState::Ready {
        bail!("remote workspace {remote_workspace_id} must be ready before opening a browser");
    }
    if let Some(existing) = store.list_browsers()?.into_iter().find(|browser| {
        browser.workspace_id == workspace.workspace_id
            && browser.surface_id == surface_id
            && !browser.state.is_terminal()
    }) {
        bail!(
            "surface {surface_id} already owns browser {}",
            existing.browser_surface_id
        );
    }

    let browser_surface_id = requested_id
        .map(str::to_string)
        .unwrap_or_else(|| format!("browser-{}", Uuid::new_v4()));
    let profile_id = browser_surface_id.clone();
    let profile_path = store.browser_profile_path(&profile_id)?;
    let proxy = proxy::open(
        store,
        &workspace.target_id,
        &workspace.workspace_id,
        Some(surface_id.to_string()),
        None,
        allow_production,
    )?;
    let now = now_ms();
    let browser = BrowserSurfaceSession {
        schema_version: COMPUTE_SCHEMA_VERSION,
        browser_surface_id,
        remote_workspace_id: remote_workspace_id.to_string(),
        workspace_id: workspace.workspace_id,
        surface_id: surface_id.to_string(),
        target_id: workspace.target_id,
        environment_id: workspace.environment_id,
        proxy_id: proxy.proxy_id.clone(),
        profile_id,
        user_data_folder: profile_path.to_string_lossy().into_owned(),
        // The proxy is ready, but the native controller still needs to report
        // successful creation before callers mark this surface Ready.
        state: BrowserSurfaceState::Starting,
        current_url: url.clone(),
        navigation_history: vec![url],
        history_index: 0,
        persistent,
        last_error: None,
        created_at_ms: now,
        updated_at_ms: now,
    };
    match store.save_browser("browser.open", browser) {
        Ok(browser) => Ok(browser),
        Err(error) => {
            let _ = proxy::close(store, &proxy.proxy_id);
            Err(error)
        }
    }
}

pub fn set_state(
    store: &ComputeStore,
    id: &str,
    state: BrowserSurfaceState,
    error: Option<String>,
) -> Result<BrowserSurfaceSession> {
    let mut browser = store.get_browser(id)?;
    if browser.state.is_terminal() && browser.state != state {
        bail!("browser surface {id} is already terminal");
    }
    if state == BrowserSurfaceState::Ready {
        let proxy = store.get_proxy(&browser.proxy_id)?;
        if proxy.state != RemoteProxyState::Ready {
            bail!("browser surface {id} cannot become ready without its proxy");
        }
    }
    browser.state = state;
    browser.last_error = error;
    store.save_browser("browser.state", browser)
}

pub fn navigate(store: &ComputeStore, id: &str, url: &str) -> Result<BrowserSurfaceSession> {
    let url = validate_navigation_url(url)?;
    let mut browser = store.get_browser(id)?;
    ensure_active(&browser)?;
    browser.state = BrowserSurfaceState::Navigating;
    browser.last_error = None;
    if browser.current_url != url {
        browser
            .navigation_history
            .truncate(browser.history_index.saturating_add(1));
        browser.navigation_history.push(url.clone());
        if browser.navigation_history.len() > MAX_HISTORY_ENTRIES {
            let overflow = browser.navigation_history.len() - MAX_HISTORY_ENTRIES;
            browser.navigation_history.drain(..overflow);
        }
        browser.history_index = browser.navigation_history.len() - 1;
        browser.current_url = url;
    }
    store.save_browser("browser.navigate", browser)
}

pub fn move_history(store: &ComputeStore, id: &str, delta: i32) -> Result<BrowserSurfaceSession> {
    if delta == 0 {
        bail!("browser history delta must not be zero");
    }
    let mut browser = store.get_browser(id)?;
    ensure_active(&browser)?;
    let target = browser.history_index as i64 + i64::from(delta);
    if target < 0 || target >= browser.navigation_history.len() as i64 {
        bail!("browser history has no entry at the requested offset");
    }
    browser.history_index = target as usize;
    browser.current_url = browser.navigation_history[browser.history_index].clone();
    browser.state = BrowserSurfaceState::Navigating;
    browser.last_error = None;
    store.save_browser("browser.history", browser)
}

pub fn close(store: &ComputeStore, id: &str) -> Result<BrowserSurfaceSession> {
    let mut browser = store.get_browser(id)?;
    if browser.state == BrowserSurfaceState::Closed {
        return Ok(browser);
    }
    browser.state = BrowserSurfaceState::Closing;
    browser.last_error = None;
    browser = store.save_browser("browser.close", browser)?;
    match proxy::close(store, &browser.proxy_id) {
        Ok(_) => {
            browser.state = BrowserSurfaceState::Closed;
            browser.last_error = None;
            store.save_browser("browser.close", browser)
        }
        Err(error) => {
            browser.state = BrowserSurfaceState::Failed;
            browser.last_error = Some(format!("browser proxy shutdown failed: {error:#}"));
            let failed = store.save_browser("browser.close", browser)?;
            Err(error).context(format!(
                "browser surface {} failed closed",
                failed.browser_surface_id
            ))
        }
    }
}

/// Replace a failed/stopped proxy while preserving the browser profile and
/// navigation identity. The native host must recreate its controller and
/// report `Ready` after this returns.
pub fn recover(
    store: &ComputeStore,
    id: &str,
    allow_production: bool,
) -> Result<BrowserSurfaceSession> {
    let mut browser = store.get_browser(id)?;
    let workspace = store.get_remote_workspace(&browser.remote_workspace_id)?;
    if workspace.state != RemoteWorkspaceState::Ready {
        bail!(
            "browser surface {id} cannot recover while remote workspace {} is not ready",
            workspace.remote_workspace_id
        );
    }
    if let Ok(existing) = store.get_proxy(&browser.proxy_id) {
        if existing.state == RemoteProxyState::Ready {
            browser.state = BrowserSurfaceState::Starting;
            browser.last_error = None;
            return store.save_browser("browser.recover", browser);
        }
    }
    if let Ok(previous) = store.get_proxy(&browser.proxy_id) {
        if !previous.state.is_terminal() {
            let _ = proxy::close(store, &previous.proxy_id);
        }
    }
    let replacement = proxy::open(
        store,
        &browser.target_id,
        &browser.workspace_id,
        Some(browser.surface_id.clone()),
        None,
        allow_production,
    )?;
    browser.proxy_id = replacement.proxy_id;
    browser.state = BrowserSurfaceState::Starting;
    browser.last_error = None;
    store.save_browser("browser.recover", browser)
}

pub fn reconcile(
    store: &ComputeStore,
    stale_after: Duration,
) -> Result<Vec<BrowserSurfaceSession>> {
    let _ = proxy::reconcile(store, stale_after)?;
    let mut changed = Vec::new();
    for mut browser in store.list_browsers()? {
        if browser.state.is_terminal() {
            continue;
        }
        let proxy = store.get_proxy(&browser.proxy_id)?;
        if proxy.state == RemoteProxyState::Ready {
            continue;
        }
        browser.state = if matches!(
            proxy.state,
            RemoteProxyState::Starting | RemoteProxyState::Stopping
        ) {
            BrowserSurfaceState::Reconnecting
        } else {
            BrowserSurfaceState::Failed
        };
        browser.last_error = Some(
            proxy
                .error
                .unwrap_or_else(|| format!("browser proxy entered state {:?}", proxy.state)),
        );
        changed.push(store.save_browser("browser.reconcile", browser)?);
    }
    Ok(changed)
}

pub fn validate_navigation_url(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_URL_BYTES {
        bail!("browser URL must contain 1-{MAX_URL_BYTES} bytes");
    }
    if value.chars().any(char::is_control) {
        bail!("browser URL must not contain control characters");
    }
    let authority = value
        .strip_prefix("https://")
        .or_else(|| value.strip_prefix("http://"))
        .context("browser URL must use http:// or https://")?;
    let authority = authority.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        bail!("browser URL requires a host");
    }
    if authority.contains('@') {
        bail!("browser URL must not embed credentials");
    }
    Ok(value.to_string())
}

fn ensure_active(browser: &BrowserSurfaceSession) -> Result<()> {
    if browser.state.is_terminal() || browser.state == BrowserSurfaceState::Closing {
        bail!(
            "browser surface {} is not active",
            browser.browser_surface_id
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_accepts_http_and_https_only() {
        assert_eq!(
            validate_navigation_url("https://example.com/path").unwrap(),
            "https://example.com/path"
        );
        assert!(validate_navigation_url("file:///etc/passwd").is_err());
        assert!(validate_navigation_url("javascript:alert(1)").is_err());
        assert!(validate_navigation_url("https://user:secret@example.com").is_err());
        assert!(validate_navigation_url("https://").is_err());
    }
}
