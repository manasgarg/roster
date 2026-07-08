//! The policy schema (mirrors `src/schema.ts`) plus the runtime GovernedRequest
//! the judge evaluates and the call log records. See docs/rust-port.md (P2).

use serde::Deserialize;
use std::collections::HashMap;

/// A JSON field that may be a single value or an array of them.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    pub fn values(&self) -> Vec<&T> {
        match self {
            OneOrMany::One(v) => vec![v],
            OneOrMany::Many(vs) => vs.iter().collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Allow,
    Deny,
    Tunnel,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Allow => "allow",
            Verdict::Deny => "deny",
            Verdict::Tunnel => "tunnel",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Match {
    pub protocol: Option<OneOrMany<String>>,
    pub host: Option<OneOrMany<String>>,
    pub port: Option<OneOrMany<u16>>,
    pub method: Option<OneOrMany<String>>,
    #[serde(rename = "pathPrefix")]
    pub path_prefix: Option<String>,
    #[serde(rename = "headerContains")]
    pub header_contains: Option<HashMap<String, String>>,
    #[serde(rename = "maxBodySize")]
    pub max_body_size: Option<u64>,
    pub mcp: Option<McpMatch>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct McpMatch {
    pub method: Option<OneOrMany<String>>,
    pub tool: Option<OneOrMany<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Inject {
    pub credential: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub name: String,
    #[serde(default)]
    pub r#match: Match,
    pub verdict: Verdict,
    #[serde(default)]
    pub inject: Option<Inject>,
    /// The scope this rule governs (ancestor of the subject). Defaults to "org"
    /// (fleet-wide) for hand-authored/legacy rules.
    #[serde(default = "org_scope")]
    pub scope: String,
}

fn org_scope() -> String {
    "org".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct Policy {
    pub rules: Vec<Rule>,
}

impl Policy {
    pub fn empty() -> Policy {
        Policy { rules: Vec::new() }
    }
    pub fn rule(&self, name: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| r.name == name)
    }
}

/// Lifted MCP terms from a JSON-RPC body.
#[derive(Debug, Clone)]
pub struct Mcp {
    pub method: String,
    pub tool: Option<String>,
}

/// What the gateway saw, phrased as the judge's question.
#[derive(Debug, Clone)]
pub struct GovernedRequest {
    pub worker: Option<String>,
    pub protocol: String,
    pub method: String,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub query: String,
    pub headers: HashMap<String, String>,
    pub body_size: u64,
    pub mcp: Option<Mcp>,
}
