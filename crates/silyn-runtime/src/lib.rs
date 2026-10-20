//! Agent runtime (W3): event loop, budgets, checkpoints, tool broker.
//!
//! The runtime never trusts model output for authorization decisions. Every
//! tool call is checked by the broker against the run's capability set.

pub mod budget;
pub mod control;
pub mod detect;
pub mod retry;
pub mod run;
pub mod state;
pub mod tools;

pub use budget::{BudgetError, BudgetSpec, BudgetTracker};
pub use control::{RunControl, cancel_file_path};
pub use detect::{LoopDetector, fingerprint_tool_call};
pub use retry::RetryPolicy;
pub use run::{RunOptions, RunOutcome, Runtime};
pub use tools::{AuditEvent, AuditSink, MemoryAuditSink, ToolBroker};
