//! Operator configuration for explicitly registered client-executed tools.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DynamicToolNamespaceConfig {
    pub name: String,
    pub description: String,
    pub tools: Vec<DynamicToolConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DynamicToolConfig {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// Absolute executable path; arguments are passed directly without a shell.
    pub command: PathBuf,
    /// Explicit executor working directory, independent of any thread worktree.
    pub cwd: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

fn default_timeout() -> u64 {
    30_000
}
