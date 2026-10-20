//! Protocol traits implemented by runtime subsystems.
//!
//! These signatures are frozen for W0. Implementations land in W1–W5 behind
//! these traits; callers must not depend on concrete types.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::SilynError;
use crate::event::VerifiedOutcome;
use crate::ids::{CandidateId, MemoryId, RunId, StepId};

/// Capability strings enforced by the tool broker.
pub mod capabilities {
    pub const FS_READ: &str = "fs.read";
    pub const FS_LIST: &str = "fs.list";
    pub const DATA_PROFILE: &str = "data.profile";
    pub const REPORT_WRITE: &str = "report.write";
}

// ---------------------------------------------------------------------------
// Model gateway
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelResponse {
    pub content: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, SilynError>;
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScope {
    Session,
    Agent,
    User,
    Shared,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryType {
    Working,
    Episodic,
    Semantic,
    Procedural,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryQuery {
    pub text: String,
    pub limit: usize,
    #[serde(default)]
    pub types: Vec<MemoryType>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryHit {
    pub memory_id: MemoryId,
    pub summary: String,
    pub score: f32,
    pub source_refs: Vec<String>,
}

#[async_trait]
pub trait MemoryRepository: Send + Sync {
    async fn search(
        &self,
        scope: MemoryScope,
        query: MemoryQuery,
    ) -> Result<Vec<MemoryHit>, SilynError>;
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizedToolContext {
    pub run_id: RunId,
    pub step_id: StepId,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: String,
    pub tool: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(
        &self,
        context: AuthorizedToolContext,
        call: ToolCall,
    ) -> Result<ToolResult, SilynError>;
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSnapshot {
    pub run_id: RunId,
    pub objective: String,
    pub artifact_refs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeEvidence {
    pub outcome: VerifiedOutcome,
    pub evidence_refs: Vec<String>,
    /// Heuristic confidence in `[0, 1]`. Not a calibrated correctness rate.
    pub confidence: f32,
}

#[async_trait]
pub trait OutcomeVerifier: Send + Sync {
    async fn verify(&self, run: RunSnapshot) -> Result<OutcomeEvidence, SilynError>;
}

// ---------------------------------------------------------------------------
// Evolution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluationReport {
    pub candidate_id: CandidateId,
    pub suite_version: String,
    pub baseline_id: String,
    pub metrics: serde_json::Value,
}

#[async_trait]
pub trait EvolutionEvaluator: Send + Sync {
    async fn evaluate(
        &self,
        candidate: CandidateId,
        suite: String,
    ) -> Result<EvaluationReport, SilynError>;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotionPolicy {
    pub allow_low_risk_auto: bool,
    pub min_gain_pp: f64,
    pub max_cost_ratio: f64,
    pub max_regression_pp: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PromotionDecision {
    Promote,
    AwaitApproval,
    Reject { reason: String },
}

#[async_trait]
pub trait PromotionGate: Send + Sync {
    async fn decide(
        &self,
        report: EvaluationReport,
        policy: PromotionPolicy,
    ) -> Result<PromotionDecision, SilynError>;
}
