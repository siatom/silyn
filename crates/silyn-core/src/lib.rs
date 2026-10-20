//! Silyn domain kernel.
//!
//! This crate is the frozen protocol layer: identifiers, lifecycle state machines,
//! error classes, run events and the traits every subsystem implements.
//!
//! Contract rules (see `docs/architecture/adr/0002-data-and-events.md`):
//! - types in this crate are versioned by schema, never edited in place after merge;
//! - no dependency on storage, LLM, runtime or growth crates;
//! - behavior that could weaken validation or permissions does not belong here.

pub mod contracts;
pub mod error;
pub mod event;
pub mod ids;
pub mod state;

pub use error::{ErrorClass, SilynError};
