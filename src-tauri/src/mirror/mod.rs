//! Saved source selection and source configuration; no transport dependency.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateSource {
    #[default]
    Git,
    Mirrorchyan,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MirrorConfig {
    pub resource_id: String,
    #[serde(default = "stable_channel")]
    pub stable_channel: String,
    #[serde(default)]
    pub prerelease_channel: Option<String>,
}
impl MirrorConfig {
    pub(crate) fn validate_resource_id(&self) -> anyhow::Result<()> {
        if self.resource_id.is_empty()
            || !self
                .resource_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        {
            anyhow::bail!("Invalid MirrorChyan resource_id");
        }
        Ok(())
    }
}
fn stable_channel() -> String {
    "stable".into()
}

pub mod api;
pub mod credentials;
pub mod download;
pub(crate) mod service;

pub mod archive;

pub(crate) mod installation;
