//! Agent runtime (W3): event loop, budgets, checkpoints, tool broker.
//!
//! The runtime never trusts model output for authorization decisions. Every
//! tool call is checked by the broker against the run's capability set.

pub mod tools;

pub use tools::{AuditEvent, AuditSink, MemoryAuditSink, ToolBroker};
