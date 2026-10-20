//! Structured, redacted `tracing` spans for the run loop.
//!
//! The contract is frozen in `docs/architecture/observability.md`: a closed set
//! of six span names and a per-span allowlist of fields. This module is the only
//! place that names those spans and fields, so a field cannot be added by
//! accident and a prompt, tool argument or memory body cannot be attached with
//! `Debug`.
//!
//! Three rules make the module safe by construction:
//!
//! - every constructor declares the full field list from the contract, marking
//!   optional fields as [`tracing::field::Empty`]; call sites only ever
//!   [`record`](tracing::Span::record) values that already passed [`redact`];
//! - string values are scrubbed (API keys, bearer tokens) and truncated to
//!   [`MAX_FIELD_BYTES`] before they reach a subscriber;
//! - nothing here installs a subscriber, so without one every span is disabled
//!   and the run loop behaves exactly as it did before W7-03b.
//!
//! Never pass model messages, tool arguments, tool output, memory summaries,
//! file contents, absolute paths or raw error messages to a span field. The
//! journal and tool ledger remain the durable record; spans carry IDs, states
//! and metrics only.

use std::time::Duration;

use silyn_core::SilynError;
use silyn_core::error::ErrorClass;
use tracing::{Level, Span, field, span};

/// One run, from its first transition to its terminal status.
pub const SPAN_RUN: &str = "run";
/// One loop iteration: a model turn or a tool resolution.
pub const SPAN_STEP: &str = "step";
/// One provider request, including retries as a field.
pub const SPAN_MODEL: &str = "model";
/// One broker-authorized tool execution.
pub const SPAN_TOOL: &str = "tool";
/// Persisting a checkpoint or recovering from one.
pub const SPAN_CHECKPOINT: &str = "checkpoint";
/// Outcome verification over the run's artifacts.
pub const SPAN_VERIFY: &str = "verify";

/// The closed set of span names, in contract order.
pub const SPAN_NAMES: &[&str] = &[
    SPAN_RUN,
    SPAN_STEP,
    SPAN_MODEL,
    SPAN_TOOL,
    SPAN_CHECKPOINT,
    SPAN_VERIFY,
];

/// Run identifier (opaque).
pub const FIELD_RUN_ID: &str = "run_id";
/// Task identifier (opaque).
pub const FIELD_TASK_ID: &str = "task_id";
/// Agent identifier (opaque).
pub const FIELD_AGENT_ID: &str = "agent_id";
/// Active policy version (opaque).
pub const FIELD_POLICY_VERSION: &str = "policy_version";
/// Model identifier reported by the provider.
pub const FIELD_MODEL_ID: &str = "model_id";
/// Lifecycle state or terminal status, using the JSON wire names.
pub const FIELD_STATUS: &str = "status";
/// Number of steps the run consumed.
pub const FIELD_STEP_COUNT: &str = "step_count";
/// Input tokens reported by the provider.
pub const FIELD_INPUT_TOKENS: &str = "input_tokens";
/// Output tokens reported by the provider.
pub const FIELD_OUTPUT_TOKENS: &str = "output_tokens";
/// Accumulated cost in US dollars, as a number.
pub const FIELD_COST_USD: &str = "cost_usd";
/// Elapsed time of the operation in whole milliseconds.
pub const FIELD_DURATION_MS: &str = "duration_ms";
/// Step identifier (opaque).
pub const FIELD_STEP_ID: &str = "step_id";
/// One-based step index inside the run.
pub const FIELD_STEP_INDEX: &str = "step_index";
/// Step kind: `model` or `tool`.
pub const FIELD_KIND: &str = "kind";
/// Provider name, when the runtime knows it.
pub const FIELD_PROVIDER: &str = "provider";
/// Model the caller asked for, before provider aliasing.
pub const FIELD_REQUESTED_MODEL: &str = "requested_model";
/// Number of retries after the first attempt.
pub const FIELD_RETRY_COUNT: &str = "retry_count";
/// Broker tool name, e.g. `fs.read`.
pub const FIELD_TOOL_NAME: &str = "tool_name";
/// Capability that authorized the call.
pub const FIELD_CAPABILITY: &str = "capability";
/// Tool ledger call identifier (opaque).
pub const FIELD_CALL_ID: &str = "call_id";
/// Ledger state: `started`, `succeeded`, `failed` or `indeterminate`.
pub const FIELD_STATE: &str = "state";
/// Opaque ledger reference. Never file content, so the runtime omits it.
pub const FIELD_RESULT_REF: &str = "result_ref";
/// Error class, never the error message.
pub const FIELD_ERROR_CLASS: &str = "error_class";
/// Checkpoint schema version.
pub const FIELD_SCHEMA_VERSION: &str = "schema_version";
/// First 12 hex characters of the checkpoint checksum.
pub const FIELD_CHECKSUM_PREFIX: &str = "checksum_prefix";
/// Checkpoint direction: `write` or `read`.
pub const FIELD_DIRECTION: &str = "direction";
/// Verifier name, when the runtime knows it.
pub const FIELD_CHECKER: &str = "checker";
/// Verifier outcome: `success`, `failed` or `unknown`.
pub const FIELD_OUTCOME: &str = "outcome";
/// Number of evidence references the verifier returned.
pub const FIELD_EVIDENCE_COUNT: &str = "evidence_count";

/// Fields allowed on a `run` span.
pub const RUN_FIELDS: &[&str] = &[
    FIELD_RUN_ID,
    FIELD_TASK_ID,
    FIELD_AGENT_ID,
    FIELD_POLICY_VERSION,
    FIELD_MODEL_ID,
    FIELD_STATUS,
    FIELD_STEP_COUNT,
    FIELD_INPUT_TOKENS,
    FIELD_OUTPUT_TOKENS,
    FIELD_COST_USD,
    FIELD_DURATION_MS,
];

/// Fields allowed on a `step` span.
pub const STEP_FIELDS: &[&str] = &[
    FIELD_RUN_ID,
    FIELD_STEP_ID,
    FIELD_STEP_INDEX,
    FIELD_KIND,
    FIELD_STATUS,
    FIELD_DURATION_MS,
];

/// Fields allowed on a `model` span.
pub const MODEL_FIELDS: &[&str] = &[
    FIELD_RUN_ID,
    FIELD_STEP_ID,
    FIELD_PROVIDER,
    FIELD_MODEL_ID,
    FIELD_REQUESTED_MODEL,
    FIELD_STATUS,
    FIELD_INPUT_TOKENS,
    FIELD_OUTPUT_TOKENS,
    FIELD_COST_USD,
    FIELD_DURATION_MS,
    FIELD_RETRY_COUNT,
];

/// Fields allowed on a `tool` span.
pub const TOOL_FIELDS: &[&str] = &[
    FIELD_RUN_ID,
    FIELD_STEP_ID,
    FIELD_TOOL_NAME,
    FIELD_CAPABILITY,
    FIELD_CALL_ID,
    FIELD_STATE,
    FIELD_DURATION_MS,
    FIELD_RESULT_REF,
    FIELD_ERROR_CLASS,
];

/// Fields allowed on a `checkpoint` span.
pub const CHECKPOINT_FIELDS: &[&str] = &[
    FIELD_RUN_ID,
    FIELD_STEP_ID,
    FIELD_SCHEMA_VERSION,
    FIELD_CHECKSUM_PREFIX,
    FIELD_DIRECTION,
    FIELD_STATUS,
];

/// Fields allowed on a `verify` span.
pub const VERIFY_FIELDS: &[&str] = &[
    FIELD_RUN_ID,
    FIELD_CHECKER,
    FIELD_OUTCOME,
    FIELD_EVIDENCE_COUNT,
    FIELD_DURATION_MS,
];

/// The W7-03b work order names this field in shorthand; spans use
/// [`FIELD_STEP_INDEX`], the canonical name in the observability contract.
pub const FIELD_STEP_SEQ: &str = "step_seq";
/// Shorthand for [`FIELD_TOOL_NAME`]; not attached to a span.
pub const FIELD_TOOL: &str = "tool";
/// Shorthand for [`FIELD_MODEL_ID`]; not attached to a span.
pub const FIELD_MODEL: &str = "model";
/// Shorthand for [`FIELD_DURATION_MS`]; not attached to a span.
pub const FIELD_LATENCY_MS: &str = "latency_ms";

/// Every field name this module knows, canonical and shorthand.
pub const ALL_FIELDS: &[&str] = &[
    FIELD_RUN_ID,
    FIELD_TASK_ID,
    FIELD_AGENT_ID,
    FIELD_POLICY_VERSION,
    FIELD_MODEL_ID,
    FIELD_STATUS,
    FIELD_STEP_COUNT,
    FIELD_INPUT_TOKENS,
    FIELD_OUTPUT_TOKENS,
    FIELD_COST_USD,
    FIELD_DURATION_MS,
    FIELD_STEP_ID,
    FIELD_STEP_INDEX,
    FIELD_KIND,
    FIELD_PROVIDER,
    FIELD_REQUESTED_MODEL,
    FIELD_RETRY_COUNT,
    FIELD_TOOL_NAME,
    FIELD_CAPABILITY,
    FIELD_CALL_ID,
    FIELD_STATE,
    FIELD_RESULT_REF,
    FIELD_ERROR_CLASS,
    FIELD_SCHEMA_VERSION,
    FIELD_CHECKSUM_PREFIX,
    FIELD_DIRECTION,
    FIELD_CHECKER,
    FIELD_OUTCOME,
    FIELD_EVIDENCE_COUNT,
    FIELD_STEP_SEQ,
    FIELD_TOOL,
    FIELD_MODEL,
    FIELD_LATENCY_MS,
];

/// Step kind for a model turn.
pub const KIND_MODEL: &str = "model";
/// Step kind for a tool resolution.
pub const KIND_TOOL: &str = "tool";

/// Checkpoint that was written.
pub const DIRECTION_WRITE: &str = "write";

/// Tool ledger state before the side effect.
pub const STATE_STARTED: &str = "started";
/// Tool ledger state after a successful execution.
pub const STATE_SUCCEEDED: &str = "succeeded";
/// Tool ledger state after a reported failure.
pub const STATE_FAILED: &str = "failed";
/// Tool ledger state when the side effect may have happened.
pub const STATE_INDETERMINATE: &str = "indeterminate";

/// Verifier reported success.
pub const OUTCOME_SUCCESS: &str = "success";
/// Verifier reported failure.
pub const OUTCOME_FAILED: &str = "failed";
/// Verifier could not decide.
pub const OUTCOME_UNKNOWN: &str = "unknown";

/// Status while the operation is in flight.
pub const STATUS_RUNNING: &str = "running";
/// Status after the operation finished.
pub const STATUS_COMPLETED: &str = "completed";
/// Status after the operation failed.
pub const STATUS_FAILED: &str = "failed";
/// Status after the operation was cancelled.
pub const STATUS_CANCELLED: &str = "cancelled";

/// Hard limit for one span field value, in bytes.
pub const MAX_FIELD_BYTES: usize = 200;

/// Marker written in place of a removed secret.
pub const REDACTED: &str = "[REDACTED]";

/// Token and cost counters attached to one model call or run.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// `None` when the call site does not know the price table.
    pub cost_usd: Option<f64>,
}

/// Scrubs secret-shaped values and bounds the result to [`MAX_FIELD_BYTES`].
///
/// The returned string is always valid UTF-8 and never longer than
/// [`MAX_FIELD_BYTES`] bytes. Removed: `sk-...` keys, the token after
/// `Bearer `, and any value assigned to `api_key=`, `api_key:` or
/// `DEEPSEEK_API_KEY=`, including the quoted JSON forms.
#[must_use]
pub fn redact(value: &str) -> String {
    truncate_utf8(scrub(value), MAX_FIELD_BYTES)
}

/// Maps an error to the contract's bounded `error_class` value.
#[must_use]
pub fn error_class(error: &SilynError) -> &'static str {
    match error.class() {
        ErrorClass::Transient => "transient",
        ErrorClass::Permanent => "permanent",
        ErrorClass::PolicyDenied => "policy_denied",
        ErrorClass::Indeterminate => "indeterminate",
    }
}

/// Root span for one run.
#[must_use]
pub fn run_span(run_id: &str, task_id: &str, policy_version: &str, model_id: &str) -> Span {
    span!(
        Level::INFO,
        SPAN_RUN,
        run_id = %redact(run_id),
        task_id = %redact(task_id),
        agent_id = field::Empty,
        policy_version = %redact(policy_version),
        model_id = %redact(model_id),
        status = field::Empty,
        step_count = field::Empty,
        input_tokens = field::Empty,
        output_tokens = field::Empty,
        cost_usd = field::Empty,
        duration_ms = field::Empty,
    )
}

/// Child span of `run` for one loop iteration.
#[must_use]
pub fn step_span(
    parent: &Span,
    run_id: &str,
    step_id: Option<&str>,
    step_index: u64,
    kind: &str,
) -> Span {
    let span = span!(
        parent: parent,
        Level::INFO,
        SPAN_STEP,
        run_id = %redact(run_id),
        step_id = field::Empty,
        step_index = step_index,
        kind = %redact(kind),
        status = field::Empty,
        duration_ms = field::Empty,
    );
    if let Some(step_id) = step_id {
        span.record(FIELD_STEP_ID, redact(step_id));
    }
    span
}

/// Child span of `step` for one provider request.
#[must_use]
pub fn model_span(parent: &Span, run_id: &str, requested_model: &str) -> Span {
    span!(
        parent: parent,
        Level::INFO,
        SPAN_MODEL,
        run_id = %redact(run_id),
        step_id = field::Empty,
        provider = field::Empty,
        model_id = field::Empty,
        requested_model = %redact(requested_model),
        status = field::Empty,
        input_tokens = field::Empty,
        output_tokens = field::Empty,
        cost_usd = field::Empty,
        duration_ms = field::Empty,
        retry_count = field::Empty,
    )
}

/// Child span of `step` for one broker-authorized tool execution.
#[must_use]
pub fn tool_span(
    parent: &Span,
    run_id: &str,
    step_id: Option<&str>,
    tool_name: &str,
    capability: Option<&str>,
    call_id: &str,
) -> Span {
    let span = span!(
        parent: parent,
        Level::INFO,
        SPAN_TOOL,
        run_id = %redact(run_id),
        step_id = field::Empty,
        tool_name = %redact(tool_name),
        capability = field::Empty,
        call_id = %redact(call_id),
        state = STATE_STARTED,
        duration_ms = field::Empty,
        result_ref = field::Empty,
        error_class = field::Empty,
    );
    if let Some(step_id) = step_id {
        span.record(FIELD_STEP_ID, redact(step_id));
    }
    if let Some(capability) = capability {
        span.record(FIELD_CAPABILITY, redact(capability));
    }
    span
}

/// Child span of `run` for one checkpoint write or read.
#[must_use]
pub fn checkpoint_span(
    parent: &Span,
    run_id: &str,
    step_id: Option<&str>,
    schema_version: i64,
    checksum_prefix: &str,
    direction: &str,
) -> Span {
    let span = span!(
        parent: parent,
        Level::INFO,
        SPAN_CHECKPOINT,
        run_id = %redact(run_id),
        step_id = field::Empty,
        schema_version = schema_version,
        checksum_prefix = %redact(checksum_prefix),
        direction = %redact(direction),
        status = field::Empty,
    );
    if let Some(step_id) = step_id {
        span.record(FIELD_STEP_ID, redact(step_id));
    }
    span
}

/// Child span of `run` for one verifier invocation.
#[must_use]
pub fn verify_span(parent: &Span, run_id: &str) -> Span {
    span!(
        parent: parent,
        Level::INFO,
        SPAN_VERIFY,
        run_id = %redact(run_id),
        checker = field::Empty,
        outcome = field::Empty,
        evidence_count = field::Empty,
        duration_ms = field::Empty,
    )
}

/// Records the terminal fields of a finished run.
pub fn record_run(span: &Span, status: &str, step_count: u32, usage: Usage, elapsed: Duration) {
    span.record(FIELD_STATUS, redact(status));
    span.record(FIELD_STEP_COUNT, step_count);
    span.record(FIELD_INPUT_TOKENS, usage.input_tokens);
    span.record(FIELD_OUTPUT_TOKENS, usage.output_tokens);
    if let Some(cost_usd) = usage.cost_usd {
        span.record(FIELD_COST_USD, cost_usd);
    }
    span.record(FIELD_DURATION_MS, millis(elapsed));
}

/// Records the elapsed time of a run that ended with an error.
///
/// The error message is deliberately dropped: it may contain a path, a provider
/// body or a key. The journal keeps the bounded reason.
pub fn record_run_error(span: &Span, elapsed: Duration) {
    span.record(FIELD_DURATION_MS, millis(elapsed));
}

/// Records the terminal fields of one step.
pub fn record_step(span: &Span, status: &str, elapsed: Duration) {
    span.record(FIELD_STATUS, redact(status));
    span.record(FIELD_DURATION_MS, millis(elapsed));
}

/// Records the terminal fields of one provider request.
pub fn record_model(
    span: &Span,
    status: &str,
    model_id: Option<&str>,
    usage: Usage,
    retry_count: u32,
    elapsed: Duration,
) {
    span.record(FIELD_STATUS, redact(status));
    if let Some(model_id) = model_id {
        span.record(FIELD_MODEL_ID, redact(model_id));
    }
    span.record(FIELD_INPUT_TOKENS, usage.input_tokens);
    span.record(FIELD_OUTPUT_TOKENS, usage.output_tokens);
    if let Some(cost_usd) = usage.cost_usd {
        span.record(FIELD_COST_USD, cost_usd);
    }
    span.record(FIELD_RETRY_COUNT, retry_count);
    span.record(FIELD_DURATION_MS, millis(elapsed));
}

/// Records the terminal state of one tool call.
pub fn record_tool(span: &Span, state: &str, error_class: Option<&str>, elapsed: Duration) {
    span.record(FIELD_STATE, redact(state));
    if let Some(error_class) = error_class {
        span.record(FIELD_ERROR_CLASS, redact(error_class));
    }
    span.record(FIELD_DURATION_MS, millis(elapsed));
}

/// Records the terminal status of one checkpoint.
pub fn record_checkpoint(span: &Span, status: &str) {
    span.record(FIELD_STATUS, redact(status));
}

/// Records the terminal fields of one verifier invocation.
pub fn record_verify(span: &Span, outcome: &str, evidence_count: usize, elapsed: Duration) {
    span.record(FIELD_OUTCOME, redact(outcome));
    span.record(
        FIELD_EVIDENCE_COUNT,
        u64::try_from(evidence_count).unwrap_or(u64::MAX),
    );
    span.record(FIELD_DURATION_MS, millis(elapsed));
}

fn millis(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

fn is_secret_char(character: char) -> bool {
    character.is_ascii_alphanumeric()
        || matches!(character, '-' | '_' | '.' | '~' | '+' | '/' | '=')
}

fn scrub(value: &str) -> String {
    // ASCII-only lowercasing keeps byte offsets aligned with `value`, so the
    // needles can be matched case-insensitively without a second buffer.
    let lower = value.to_ascii_lowercase();
    let mut out = String::with_capacity(value.len());
    let mut index = 0;

    while index < value.len() {
        let remaining = &lower[index..];

        if remaining.starts_with("bearer ") {
            out.push_str(&value[index..index + "bearer ".len()]);
            index += "bearer ".len();
            let start = index;
            while index < value.len() {
                let character = next_char(value, index);
                if is_secret_char(character) {
                    index += character.len_utf8();
                } else {
                    break;
                }
            }
            if index > start {
                out.push_str(REDACTED);
            }
            continue;
        }

        if remaining.starts_with("sk-") {
            index += "sk-".len();
            while index < value.len() {
                let character = next_char(value, index);
                if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                    index += character.len_utf8();
                } else {
                    break;
                }
            }
            out.push_str(REDACTED);
            continue;
        }

        if remaining.starts_with("api_key") {
            let after_key = index + "api_key".len();
            let mut separator = after_key;
            while separator < value.len() {
                let character = next_char(value, separator);
                if matches!(character, ' ' | '\t' | '"' | '\'') {
                    separator += character.len_utf8();
                } else {
                    break;
                }
            }
            if separator < value.len() && matches!(value.as_bytes()[separator], b'=' | b':') {
                let mut value_start = separator + 1;
                while value_start < value.len()
                    && matches!(next_char(value, value_start), ' ' | '\t')
                {
                    value_start += 1;
                }
                out.push_str(&value[index..value_start]);
                if value_start < value.len() {
                    let opener = value.as_bytes()[value_start];
                    if opener == b'"' || opener == b'\'' {
                        out.push(char::from(opener));
                        out.push_str(REDACTED);
                        let mut end = value_start + 1;
                        while end < value.len() && value.as_bytes()[end] != opener {
                            end += 1;
                        }
                        if end < value.len() {
                            out.push(char::from(opener));
                            end += 1;
                        }
                        index = end;
                        continue;
                    }
                    let mut end = value_start;
                    while end < value.len() {
                        let character = next_char(value, end);
                        if character.is_whitespace()
                            || matches!(character, ',' | ';' | '&' | '"' | '\'')
                        {
                            break;
                        }
                        end += character.len_utf8();
                    }
                    out.push_str(REDACTED);
                    index = end;
                    continue;
                }
                index = value_start;
                continue;
            }
        }

        let character = next_char(value, index);
        out.push(character);
        index += character.len_utf8();
    }

    out
}

fn next_char(value: &str, index: usize) -> char {
    value[index..]
        .chars()
        .next()
        .expect("index is a char boundary")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENSITIVE_WORDS: [&str; 5] = ["prompt", "content", "memory", "api_key", "secret"];

    fn doc() -> String {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/architecture/observability.md"
        );
        std::fs::read_to_string(path).expect("read docs/architecture/observability.md")
    }

    /// Markdown table rows under `heading`, as trimmed cells.
    fn section_rows(text: &str, heading: &str) -> Vec<Vec<String>> {
        let mut rows = Vec::new();
        let mut inside = false;
        for line in text.lines() {
            if line.starts_with("## ") {
                inside = line.trim() == heading;
                continue;
            }
            if !inside || !line.starts_with('|') {
                continue;
            }
            let cells: Vec<String> = line
                .trim()
                .trim_matches('|')
                .split('|')
                .map(|cell| cell.trim().to_owned())
                .collect();
            let separator = cells
                .iter()
                .all(|cell| !cell.is_empty() && cell.chars().all(|c| c == '-' || c == ':'));
            if cells[0] == "Span" || separator {
                continue;
            }
            rows.push(cells);
        }
        rows
    }

    /// Backticked tokens outside parentheses, in order.
    fn backticked(cell: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut current = String::new();
        let mut in_code = false;
        let mut depth = 0_u32;
        for character in cell.chars() {
            match character {
                '(' if !in_code => depth += 1,
                ')' if !in_code => depth = depth.saturating_sub(1),
                '`' if depth == 0 => {
                    if in_code {
                        tokens.push(std::mem::take(&mut current));
                    }
                    in_code = !in_code;
                }
                _ if in_code => current.push(character),
                _ => {}
            }
        }
        tokens
    }

    fn as_vec(fields: &[&str]) -> Vec<String> {
        fields.iter().map(|field| (*field).to_owned()).collect()
    }

    #[test]
    fn span_names_and_fields_match_observability_doc() {
        let text = doc();

        let doc_spans: Vec<String> = section_rows(&text, "## Span names")
            .iter()
            .map(|row| {
                backticked(&row[0])
                    .into_iter()
                    .next()
                    .expect("a span name in column one")
            })
            .collect();
        assert_eq!(doc_spans, as_vec(SPAN_NAMES));

        let doc_fields: Vec<(String, Vec<String>)> = section_rows(&text, "## Fields")
            .iter()
            .map(|row| {
                let name = backticked(&row[0])
                    .into_iter()
                    .next()
                    .expect("a span name in column one");
                (name, backticked(&row[1]))
            })
            .collect();

        let declared = [
            (SPAN_RUN, RUN_FIELDS),
            (SPAN_STEP, STEP_FIELDS),
            (SPAN_MODEL, MODEL_FIELDS),
            (SPAN_TOOL, TOOL_FIELDS),
            (SPAN_CHECKPOINT, CHECKPOINT_FIELDS),
            (SPAN_VERIFY, VERIFY_FIELDS),
        ];
        assert_eq!(doc_fields.len(), declared.len());
        for (span, fields) in declared {
            let (_, expected) = doc_fields
                .iter()
                .find(|(name, _)| name == span)
                .unwrap_or_else(|| panic!("doc is missing span `{span}`"));
            assert_eq!(as_vec(fields), *expected, "field list for `{span}`");
        }
    }

    #[test]
    fn field_constants_hide_sensitive_words() {
        for field in ALL_FIELDS {
            for word in SENSITIVE_WORDS {
                assert!(
                    !field.contains(word),
                    "field `{field}` contains the sensitive word `{word}`"
                );
            }
        }
        for span in SPAN_NAMES {
            for word in SENSITIVE_WORDS {
                assert!(!span.contains(word), "span `{span}` leaks `{word}`");
            }
        }

        // The work-order shorthand stays resolvable and distinct from the
        // canonical contract names.
        assert_eq!(FIELD_STEP_SEQ, "step_seq");
        assert_eq!(FIELD_TOOL, "tool");
        assert_eq!(FIELD_MODEL, "model");
        assert_eq!(FIELD_LATENCY_MS, "latency_ms");
    }

    #[test]
    fn secret_shapes_are_redacted() {
        assert_eq!(
            redact("Authorization: Bearer eyJhbGciOi.payload.signature"),
            "Authorization: Bearer [REDACTED]"
        );
        assert_eq!(redact("key sk-0123456789abcdef"), "key [REDACTED]");
        assert_eq!(redact("api_key=plain-text-value"), "api_key=[REDACTED]");
        assert_eq!(
            redact("DEEPSEEK_API_KEY=abc123"),
            "DEEPSEEK_API_KEY=[REDACTED]"
        );
        assert_eq!(
            redact("{\"api_key\": \"plain-text-value\"}"),
            "{\"api_key\": \"[REDACTED]\"}"
        );

        // Several shapes in one value, and unaffected text survives.
        let redacted = redact("call fs.read with api_key=secret123 and sk-live999");
        assert_eq!(
            redacted,
            "call fs.read with api_key=[REDACTED] and [REDACTED]"
        );
        assert!(!redacted.contains("secret123"));
        assert!(!redacted.contains("sk-live999"));

        // Key-free text is unchanged.
        assert_eq!(
            redact("run 0194f0c0 model deepseek-chat status running"),
            "run 0194f0c0 model deepseek-chat status running"
        );

        // Every value is bounded even after scrubbing.
        let long_key = format!("sk-{}", "a".repeat(500));
        assert!(redact(&long_key).len() <= MAX_FIELD_BYTES);
    }

    #[test]
    fn truncation_is_utf8_safe() {
        let ascii = "a".repeat(MAX_FIELD_BYTES + 50);
        let redacted = redact(&ascii);
        assert_eq!(redacted.len(), MAX_FIELD_BYTES);
        assert_eq!(redacted, "a".repeat(MAX_FIELD_BYTES));

        // 199 ASCII bytes plus a two-byte character: the character cannot fit,
        // so the result stops on the boundary instead of splitting it.
        let mixed = format!("{}{}", "a".repeat(MAX_FIELD_BYTES - 1), "é");
        let redacted = redact(&mixed);
        assert_eq!(redacted.len(), MAX_FIELD_BYTES - 1);
        assert!(redacted.chars().all(|character| character == 'a'));

        // A string of multi-byte characters stays valid UTF-8.
        let wide = "é".repeat(MAX_FIELD_BYTES);
        let redacted = redact(&wide);
        assert!(redacted.len() <= MAX_FIELD_BYTES);
        assert_eq!(redacted.len() % 2, 0);
        assert_eq!(redacted, "é".repeat(MAX_FIELD_BYTES / 2));

        // An exact fit is not truncated.
        assert_eq!(redact(&ascii[..MAX_FIELD_BYTES]), ascii[..MAX_FIELD_BYTES]);
    }

    #[test]
    fn error_classes_match_the_contract() {
        assert_eq!(
            error_class(&SilynError::Transient("x".to_owned())),
            "transient"
        );
        assert_eq!(
            error_class(&SilynError::Permanent("x".to_owned())),
            "permanent"
        );
        assert_eq!(
            error_class(&SilynError::PolicyDenied("x".to_owned())),
            "policy_denied"
        );
        assert_eq!(
            error_class(&SilynError::Indeterminate("x".to_owned())),
            "indeterminate"
        );
    }
}
