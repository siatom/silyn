//! Capability-gated tool broker.
//!
//! The broker is the only path from model output to side effects. Authorization
//! comes from the run's granted capability set, never from the prompt or the
//! arguments. Every decision, allowed or denied, is written to an [`AuditSink`]
//! before the result is returned.

pub mod path;

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use silyn_core::contracts::{
    AuthorizedToolContext, ToolCall, ToolExecutor, ToolResult, capabilities,
};
use silyn_core::error::SilynError;
use silyn_core::ids::{RunId, StepId};

/// Tool name for directory listing.
pub const TOOL_FS_LIST: &str = "fs.list";
/// Tool name for reading a UTF-8 text file.
pub const TOOL_FS_READ: &str = "fs.read";
/// Tool name for profiling a CSV file.
pub const TOOL_DATA_PROFILE: &str = "data.profile";
/// Tool name for writing a report below the run output directory.
pub const TOOL_REPORT_WRITE: &str = "report.write";

/// Directory, relative to the run root, that `report.write` may write into.
pub const OUTPUT_DIR: &str = "output";

/// Largest file `fs.read` will return.
pub const MAX_READ_BYTES: u64 = 1024 * 1024;

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

/// An append-only record of one broker decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    pub run_id: RunId,
    pub step_id: StepId,
    pub tool: String,
    /// `allow` or `deny`.
    pub decision: String,
    pub detail: String,
}

/// Destination for broker audit records.
///
/// Implementations must not panic on a poisoned lock; the broker records before
/// returning a result and must stay infallible.
pub trait AuditSink: Send + Sync {
    fn record(&self, event: AuditEvent);
}

/// In-memory audit sink, intended for tests and single-process runs.
#[derive(Debug, Default)]
pub struct MemoryAuditSink {
    events: Mutex<Vec<AuditEvent>>,
}

impl MemoryAuditSink {
    /// Creates an empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a snapshot of the recorded events in occurrence order.
    #[must_use]
    pub fn events(&self) -> Vec<AuditEvent> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl AuditSink for MemoryAuditSink {
    fn record(&self, event: AuditEvent) {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
    }
}

// ---------------------------------------------------------------------------
// Broker
// ---------------------------------------------------------------------------

/// Capability-gated executor for the built-in filesystem and data tools.
pub struct ToolBroker {
    root: PathBuf,
    audit: Arc<dyn AuditSink>,
}

impl ToolBroker {
    /// Creates a broker confined to `root`.
    ///
    /// The root must exist and be a directory. It is canonicalized once so every
    /// later comparison uses the same spelling as the resolved paths.
    pub fn new(root: impl AsRef<Path>, audit: Arc<dyn AuditSink>) -> Result<Self, SilynError> {
        let root = dunce::canonicalize(root.as_ref())
            .map_err(|_| SilynError::Permanent("run root is not accessible".to_owned()))?;
        if !root.is_dir() {
            return Err(SilynError::Permanent(
                "run root is not a directory".to_owned(),
            ));
        }
        Ok(Self { root, audit })
    }

    /// Returns the canonical run root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn execute_tool(
        &self,
        context: &AuthorizedToolContext,
        call: &ToolCall,
    ) -> Result<String, SilynError> {
        let capability = required_capability(&call.tool)
            .ok_or_else(|| SilynError::Permanent(format!("unknown tool `{}`", call.tool)))?;

        // Authorization happens before any argument parsing or filesystem access.
        if !context
            .capabilities
            .iter()
            .any(|granted| granted == capability)
        {
            return Err(SilynError::PolicyDenied(format!(
                "capability `{capability}` is not granted for tool `{}`",
                call.tool
            )));
        }

        match call.tool.as_str() {
            TOOL_FS_LIST => self.fs_list(call),
            TOOL_FS_READ => self.fs_read(call),
            TOOL_DATA_PROFILE => self.data_profile(call),
            TOOL_REPORT_WRITE => self.report_write(call),
            other => Err(SilynError::Permanent(format!("unknown tool `{other}`"))),
        }
    }

    fn fs_list(&self, call: &ToolCall) -> Result<String, SilynError> {
        let args: ListArgs = parse_args(call)?;
        let candidate = args.path.unwrap_or_else(|| ".".to_owned());
        let directory = path::resolve_existing(&self.root, &candidate)?;
        if !directory.is_dir() {
            return Err(SilynError::Permanent(format!(
                "tool path `{candidate}` is not a directory"
            )));
        }

        let mut names = Vec::new();
        for entry in fs::read_dir(&directory).map_err(|_| {
            SilynError::Permanent(format!("directory `{candidate}` cannot be listed"))
        })? {
            let entry = entry.map_err(|_| {
                SilynError::Permanent(format!("directory `{candidate}` cannot be listed"))
            })?;
            let name = entry.file_name().into_string().map_err(|_| {
                SilynError::Permanent(format!(
                    "directory `{candidate}` contains a non-UTF-8 entry name"
                ))
            })?;
            names.push(name);
        }
        names.sort();

        serde_json::to_string(&names).map_err(Into::into)
    }

    fn fs_read(&self, call: &ToolCall) -> Result<String, SilynError> {
        let args: ReadArgs = parse_args(call)?;
        let file = path::resolve_existing(&self.root, &args.path)?;

        let metadata = fs::metadata(&file).map_err(|_| {
            SilynError::Permanent(format!("tool path `{}` is not accessible", args.path))
        })?;
        if !metadata.is_file() {
            return Err(SilynError::Permanent(format!(
                "tool path `{}` is not a regular file",
                args.path
            )));
        }
        if metadata.len() > MAX_READ_BYTES {
            return Err(SilynError::Permanent(format!(
                "file `{}` exceeds the {MAX_READ_BYTES} byte read limit",
                args.path
            )));
        }

        let bytes = fs::read(&file).map_err(|_| {
            SilynError::Permanent(format!("tool path `{}` is not accessible", args.path))
        })?;
        if bytes.len() as u64 > MAX_READ_BYTES {
            return Err(SilynError::Permanent(format!(
                "file `{}` exceeds the {MAX_READ_BYTES} byte read limit",
                args.path
            )));
        }

        String::from_utf8(bytes)
            .map_err(|_| SilynError::Permanent(format!("file `{}` is not valid UTF-8", args.path)))
    }

    fn data_profile(&self, call: &ToolCall) -> Result<String, SilynError> {
        let args: ReadArgs = parse_args(call)?;
        let file = path::resolve_existing(&self.root, &args.path)?;

        let mut reader = csv::Reader::from_path(&file).map_err(|error| {
            SilynError::Permanent(format!("file `{}` is not readable CSV: {error}", args.path))
        })?;
        let columns: Vec<String> = reader
            .headers()
            .map_err(|error| {
                SilynError::Permanent(format!(
                    "file `{}` has no readable header row: {error}",
                    args.path
                ))
            })?
            .iter()
            .map(str::to_owned)
            .collect();

        let mut rows = 0_u64;
        let mut missing_counts = vec![0_u64; columns.len()];
        let mut seen: HashSet<Vec<String>> = HashSet::new();
        let mut duplicate_rows = 0_u64;

        for record in reader.records() {
            let record = record.map_err(|error| {
                SilynError::Permanent(format!("file `{}` is not readable CSV: {error}", args.path))
            })?;
            rows += 1;

            for (index, field) in record.iter().enumerate() {
                if field.trim().is_empty() {
                    if let Some(count) = missing_counts.get_mut(index) {
                        *count += 1;
                    }
                }
            }

            let key: Vec<String> = record.iter().map(str::to_owned).collect();
            if !seen.insert(key) {
                duplicate_rows += 1;
            }
        }

        let missing: BTreeMap<&str, u64> = columns
            .iter()
            .zip(missing_counts)
            .filter(|(_, count)| *count > 0)
            .map(|(column, count)| (column.as_str(), count))
            .collect();

        serde_json::to_string(&serde_json::json!({
            "rows": rows,
            "columns": columns,
            "missing": missing,
            "duplicate_rows": duplicate_rows,
        }))
        .map_err(Into::into)
    }

    fn report_write(&self, call: &ToolCall) -> Result<String, SilynError> {
        let args: WriteArgs = parse_args(call)?;

        let relative = path::normalize_relative(&args.path)?;
        let inside_output = matches!(
            relative.components().next(),
            Some(Component::Normal(first)) if first == OsStr::new(OUTPUT_DIR)
        );
        if !inside_output {
            return Err(SilynError::Permanent(format!(
                "report path `{}` must be inside `{OUTPUT_DIR}/`",
                args.path
            )));
        }

        let output = self.root.join(OUTPUT_DIR);
        fs::create_dir_all(&output).map_err(|_| {
            SilynError::Permanent(format!(
                "run output directory `{OUTPUT_DIR}/` is not writable"
            ))
        })?;

        let target = path::resolve_for_write(&self.root, &args.path)?;
        fs::write(&target, args.content.as_bytes()).map_err(|_| {
            SilynError::Permanent(format!("report path `{}` is not writable", args.path))
        })?;

        let reported = target.strip_prefix(&self.root).map_err(|_| {
            SilynError::Permanent(format!("report path `{}` escapes the run root", args.path))
        })?;
        Ok(reported.to_string_lossy().replace('\\', "/"))
    }

    fn record(
        &self,
        context: &AuthorizedToolContext,
        call: &ToolCall,
        decision: &str,
        detail: String,
    ) {
        self.audit.record(AuditEvent {
            run_id: context.run_id,
            step_id: context.step_id,
            tool: call.tool.clone(),
            decision: decision.to_owned(),
            detail,
        });
    }
}

#[async_trait]
impl ToolExecutor for ToolBroker {
    async fn execute(
        &self,
        context: AuthorizedToolContext,
        call: ToolCall,
    ) -> Result<ToolResult, SilynError> {
        match self.execute_tool(&context, &call) {
            Ok(content) => {
                let capability = required_capability(&call.tool).unwrap_or("unknown");
                self.record(
                    &context,
                    &call,
                    "allow",
                    format!("call_id={} capability={capability}", call.call_id),
                );
                Ok(ToolResult {
                    content,
                    is_error: false,
                })
            }
            Err(error) => {
                self.record(
                    &context,
                    &call,
                    "deny",
                    format!("call_id={} {error}", call.call_id),
                );
                Err(error)
            }
        }
    }
}

/// Returns the capability required to run `tool`, or `None` for unknown tools.
#[must_use]
pub fn required_capability(tool: &str) -> Option<&'static str> {
    match tool {
        TOOL_FS_LIST => Some(capabilities::FS_LIST),
        TOOL_FS_READ => Some(capabilities::FS_READ),
        TOOL_DATA_PROFILE => Some(capabilities::DATA_PROFILE),
        TOOL_REPORT_WRITE => Some(capabilities::REPORT_WRITE),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Argument shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
}

fn parse_args<T: DeserializeOwned>(call: &ToolCall) -> Result<T, SilynError> {
    serde_json::from_value(call.arguments.clone()).map_err(|error| {
        SilynError::Permanent(format!(
            "invalid arguments for tool `{}`: {error}",
            call.tool
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_mapping_is_explicit() {
        assert_eq!(
            required_capability(TOOL_FS_LIST),
            Some(capabilities::FS_LIST)
        );
        assert_eq!(
            required_capability(TOOL_FS_READ),
            Some(capabilities::FS_READ)
        );
        assert_eq!(
            required_capability(TOOL_DATA_PROFILE),
            Some(capabilities::DATA_PROFILE)
        );
        assert_eq!(
            required_capability(TOOL_REPORT_WRITE),
            Some(capabilities::REPORT_WRITE)
        );
        assert_eq!(required_capability("shell.exec"), None);
    }
}
