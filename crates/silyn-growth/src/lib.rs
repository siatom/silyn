//! Growth subsystems (W4–W5): memory, experience extraction, reflection,
//! candidate management and promotion.
//!
//! Boundaries: reflection may propose candidates but can never mutate an active
//! policy. Promotion is executed only by the gate.

pub mod content;
pub mod experience;
pub mod memory;
pub mod verifier;

pub use content::ContentStore;
pub use experience::{
    EvidenceStatus, ExperienceRecord, ExperienceStatus, ExperienceStore, GENERIC_LESSON,
    SUCCESS_LESSON, lesson_for_code, normalize_lesson,
};
pub use memory::{
    MemoryHitRecord, MemoryRecord, MemoryRelation, MemoryScopeFilter, MemoryStatus, MemoryStore,
    NewMemory, Provenance, PurgeReport, RelationType,
};
pub use verifier::{
    ArtifactSet, CheckResult, Checker, CompositeVerifier, ExpectedAnswerChecker,
    FileContainsChecker, VerificationPolicy,
};
