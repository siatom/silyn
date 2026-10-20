//! Growth subsystems (W4–W5): memory, experience extraction, reflection,
//! candidate management and promotion.
//!
//! Boundaries: reflection may propose candidates but can never mutate an active
//! policy. Promotion is executed only by the gate.

pub mod candidate;
pub mod content;
pub mod experience;
pub mod memory;
pub mod policy;
pub mod promotion;
pub mod verifier;

pub use candidate::{
    CandidateRecord, CandidateStore, ExpectedGain, MAX_EVIDENCE_RUNS, MAX_HYPOTHESIS_CHARS,
    MAX_OPERATIONS, PATCH_SCHEMA_VERSION, PatchOp, PatchOperation, PolicyPatch, RiskClass,
    apply_patch, validate_patch,
};
pub use content::ContentStore;
pub use experience::{
    EvidenceStatus, ExperienceRecord, ExperienceStatus, ExperienceStore, GENERIC_LESSON,
    SUCCESS_LESSON, lesson_for_code, normalize_lesson,
};
pub use memory::{
    MemoryHitRecord, MemoryRecord, MemoryRelation, MemoryScopeFilter, MemoryStatus, MemoryStore,
    NewMemory, Provenance, PurgeReport, RelationType,
};
pub use policy::{
    ContextPolicy, MAX_BM25_K, MAX_LIST_ENTRIES, MAX_MAX_EVIDENCE, MAX_MEMORY_BUDGET_RATIO,
    MAX_TEXT_BYTES, MIN_BM25_K, MIN_MAX_EVIDENCE, POLICY_SCHEMA_VERSION, PlannerPolicy,
    PolicyArtifact, PolicyStore, PolicyVerification, PolicyVersionRecord, RetrievalPolicy,
    ToolPolicy,
};
pub use promotion::{
    AUDIT_DECIDE, AUDIT_PROMOTE, AUDIT_ROLLBACK, AUDIT_ROLLBACK_DRILL, DEFAULT_MAX_COST_RATIO,
    DEFAULT_MAX_REGRESSION_PP, DEFAULT_MIN_GAIN_PP, GATE_ACTOR, PROMOTION_DECISION,
    PromotionRecord, PromotionReport, PromotionStore, VERDICT_PROMOTE, VERDICT_REJECT,
};
pub use verifier::{
    ArtifactSet, CheckResult, Checker, CompositeVerifier, ExpectedAnswerChecker,
    FileContainsChecker, VerificationPolicy,
};
