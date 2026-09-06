//! Provider-neutral, read-only forge contract. No credentials or raw provider objects.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgeAvailability {
    Ready,
    Unsupported,
    Unavailable,
    AccessUncertain,
    RateLimited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeCapabilities {
    pub summaries: bool,
    pub checks: bool,
    pub reviews: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeRepository {
    pub name: String,
    pub web_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestSummary {
    pub number: u64,
    pub title: String,
    pub draft: bool,
    pub web_url: String,
}

/// One transient page of open pull requests. Missing checks/reviews are declared
/// in capabilities, never represented as passing or approved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgePage {
    pub repository: Option<ForgeRepository>,
    pub availability: ForgeAvailability,
    pub capabilities: ForgeCapabilities,
    pub pulls: Vec<PullRequestSummary>,
    pub page: u32,
    pub next_page: Option<u32>,
    pub retry_after_seconds: Option<u64>,
}
