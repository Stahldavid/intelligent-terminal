//! Relay-specific facade over the shared SSH node bridge.

use anyhow::Result;
use serde_json::Value;

use super::node_client::RemoteNodeClient;
use super::store::ComputeStore;

pub struct RemoteRelayClient {
    inner: RemoteNodeClient,
}

impl RemoteRelayClient {
    pub async fn connect(store: &ComputeStore, target_id: &str) -> Result<Self> {
        Ok(Self {
            inner: RemoteNodeClient::connect(store, target_id, "workspace_surface_relay_v1")
                .await?,
        })
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.inner.request(method, params).await
    }

    pub async fn close(self) -> Result<()> {
        self.inner.close().await
    }
}
