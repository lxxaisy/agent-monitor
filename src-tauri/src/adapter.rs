//! Provider boundary kept independent from the dashboard and reconciler.
//! Codex is the first adapter; other local agents can implement this contract later.

#![allow(dead_code)]

use crate::codex::{CodexClient, CodexError};
use crate::model::{TaskDetail, TaskSnapshot};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AdapterCapabilities {
    pub list: bool,
    pub events: bool,
    pub progress: bool,
    pub attention: bool,
    pub open: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AdapterHealth {
    pub status: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentEvent {
    pub provider: String,
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    pub event_type: String,
}

pub trait AgentAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn capabilities(&self) -> AdapterCapabilities;
    fn discover(&self, archived: bool) -> Result<Vec<TaskDetail>, CodexError>;
    fn reconcile(&self, task_ids: &[String]) -> Result<Vec<TaskDetail>, CodexError>;
    fn subscribe(
        &self,
        on_event: Box<dyn Fn(AgentEvent) + Send + Sync>,
    ) -> Result<Box<dyn FnOnce() + Send>, CodexError>;
    fn health(&self) -> AdapterHealth;
}

pub struct CodexAdapter {
    binary: PathBuf,
    codex_home: PathBuf,
    client: Mutex<Option<CodexClient>>,
}

impl CodexAdapter {
    pub fn new(binary: impl AsRef<Path>, codex_home: impl AsRef<Path>) -> Self {
        Self {
            binary: binary.as_ref().to_path_buf(),
            codex_home: codex_home.as_ref().to_path_buf(),
            client: Mutex::new(None),
        }
    }

    fn client(&self) -> Result<std::sync::MutexGuard<'_, Option<CodexClient>>, CodexError> {
        let mut client = self.client.lock().map_err(|_| CodexError::Disconnected)?;
        if client
            .as_ref()
            .map(|value| value.is_alive())
            .unwrap_or(false)
        {
            return Ok(client);
        }
        *client = Some(CodexClient::connect(&self.binary, &self.codex_home)?);
        Ok(client)
    }
}

impl AgentAdapter for CodexAdapter {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn display_name(&self) -> &'static str {
        "Codex"
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            list: true,
            events: true,
            progress: true,
            attention: true,
            open: true,
        }
    }

    fn discover(&self, archived: bool) -> Result<Vec<TaskDetail>, CodexError> {
        self.client()?
            .as_ref()
            .expect("client initialized")
            .discover(archived)
    }

    fn reconcile(&self, task_ids: &[String]) -> Result<Vec<TaskDetail>, CodexError> {
        let client = self.client()?;
        let client = client.as_ref().expect("client initialized");
        let mut details = Vec::new();
        for task_id in task_ids {
            if let Ok(thread) = client.read_thread(task_id) {
                details.push(thread.into_detail());
            }
        }
        Ok(details)
    }

    fn subscribe(
        &self,
        _on_event: Box<dyn Fn(AgentEvent) + Send + Sync>,
    ) -> Result<Box<dyn FnOnce() + Send>, CodexError> {
        // The monitor owns the Codex event pump today. This no-op unsubscribe keeps the
        // provider contract ready for adapters that expose their own callback stream.
        Ok(Box::new(|| {}))
    }

    fn health(&self) -> AdapterHealth {
        match self.client.lock() {
            Ok(client)
                if client
                    .as_ref()
                    .map(|value| value.is_alive())
                    .unwrap_or(false) =>
            {
                AdapterHealth {
                    status: "online".into(),
                    detail: None,
                }
            }
            _ => AdapterHealth {
                status: "offline".into(),
                detail: Some("app-server 尚未连接".into()),
            },
        }
    }
}

#[allow(dead_code)]
fn _adapter_contract_uses_normalized_snapshots(_: &TaskSnapshot) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_adapter_exposes_provider_capabilities_without_ui_dependencies() {
        let adapter = CodexAdapter::new("/tmp/codex", "/tmp/.codex");
        assert_eq!(adapter.id(), "codex");
        assert_eq!(adapter.display_name(), "Codex");
        assert!(adapter.capabilities().list);
        assert!(adapter.capabilities().attention);
    }
}
