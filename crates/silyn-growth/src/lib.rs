//! Growth subsystems (W4–W5): memory, experience extraction, reflection,
//! candidate management and promotion.
//!
//! Boundaries: reflection may propose candidates but can never mutate an active
//! policy. Promotion is executed only by the gate.

pub mod content;
pub mod memory;

pub use content::ContentStore;
pub use memory::{
    MemoryHitRecord, MemoryRecord, MemoryRelation, MemoryScopeFilter, MemoryStatus, MemoryStore,
    NewMemory, Provenance, PurgeReport, RelationType,
};
