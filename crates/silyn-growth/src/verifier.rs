//! Pluggable, evidence-based outcome verification.
//!
//! A run is only `Success` when independent machine checks agree. Checkers are
//! pure functions of the run snapshot and its artifacts: they never call a
//! model, the network or a shell, so a self-assessment cannot become evidence.
//!
//! `Unknown` is a first-class result. Absence of evidence never turns into a
//! success, and an explicit failure always dominates a missing check.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use silyn_core::SilynError;
use silyn_core::contracts::{OutcomeEvidence, OutcomeVerifier, RunSnapshot};
use silyn_core::event::VerifiedOutcome;

/// Prefix for a reference whose content is carried in the reference itself.
pub const INLINE_PREFIX: &str = "inline:";
/// Prefix for a path relative to the [`ArtifactSet`] root.
pub const FILE_PREFIX: &str = "file:";

/// Upper bound on one artifact read from disk, so a huge or hostile file
/// cannot exhaust memory during verification.
const MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024;

/// Heuristic confidence reported for a fully successful verification.
const SUCCESS_CONFIDENCE: f32 = 0.8;
/// Heuristic confidence reported for a failing verification.
const FAILED_CONFIDENCE: f32 = 0.8;
/// Heuristic confidence reported when evidence is insufficient.
const UNKNOWN_CONFIDENCE: f32 = 0.2;

/// Result of one independent check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    /// The check passed and names the evidence it observed.
    Success { evidence: Vec<String> },
    /// The check failed with a stable code, a human reason and the evidence it
    /// inspected.
    Failed {
        code: String,
        reason: String,
        evidence: Vec<String>,
    },
    /// The check could not decide. Never treated as success.
    Unknown { reason: String },
}

impl CheckResult {
    /// True when the check passed.
    #[must_use]
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }

    /// True when the check failed explicitly.
    #[must_use]
    pub fn is_failed(&self) -> bool {
        matches!(self, Self::Failed { .. })
    }

    /// Evidence observed by the check, empty for `Unknown`.
    #[must_use]
    pub fn evidence(&self) -> &[String] {
        match self {
            Self::Success { evidence } | Self::Failed { evidence, .. } => evidence,
            Self::Unknown { .. } => &[],
        }
    }

    /// Maps the check result onto the run outcome.
    #[must_use]
    pub fn outcome(&self) -> VerifiedOutcome {
        match self {
            Self::Success { .. } => VerifiedOutcome::Success,
            Self::Failed { .. } => VerifiedOutcome::Failed,
            Self::Unknown { .. } => VerifiedOutcome::Unknown,
        }
    }
}

/// Artifacts a run produced, loaded once per verification.
///
/// References are resolved in three forms:
///
/// - `inline:<text>` carries the content in the reference;
/// - `file:<path>` is a path relative to the set's root and must stay inside
///   that root (no `..`, no absolute paths, no symlink escape);
/// - any other string is treated as a file path when it resolves inside the
///   sandbox and exists, otherwise as inline content (W3 compatibility).
///
/// Unreadable, oversized or sandbox-escaping references are dropped instead of
/// guessed, which surfaces as `Unknown` rather than a false success.
#[derive(Debug, Clone, Default)]
pub struct ArtifactSet {
    contents: Vec<String>,
    aliases: Vec<(String, usize)>,
}

impl ArtifactSet {
    /// Loads every readable reference once. File reads are the only I/O.
    #[must_use]
    pub fn new(root: Option<PathBuf>, refs: &[String]) -> Self {
        let mut set = Self {
            contents: Vec::new(),
            aliases: Vec::new(),
        };

        for reference in refs {
            let Some((content, resolved)) = load_reference(root.as_deref(), reference) else {
                continue;
            };
            let index = set.contents.len();
            set.contents.push(content);
            set.aliases.push((reference.clone(), index));
            if let Some(resolved) = resolved
                && resolved != *reference
            {
                set.aliases.push((resolved, index));
            }
        }

        set
    }

    /// Content for an original reference (or its resolved file path).
    #[must_use]
    pub fn get(&self, reference: &str) -> Option<&str> {
        self.aliases
            .iter()
            .find(|(alias, _)| alias == reference)
            .map(|(_, index)| self.contents[*index].as_str())
    }

    /// Contents of every loaded artifact, in reference order.
    #[must_use]
    pub fn all(&self) -> &[String] {
        &self.contents
    }

    /// Number of loaded artifacts.
    #[must_use]
    pub fn len(&self) -> usize {
        self.contents.len()
    }

    /// True when no reference could be loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.contents.is_empty()
    }
}

fn load_reference(root: Option<&Path>, reference: &str) -> Option<(String, Option<String>)> {
    if let Some(text) = reference.strip_prefix(INLINE_PREFIX) {
        return Some((text.to_owned(), None));
    }

    if let Some(path) = reference.strip_prefix(FILE_PREFIX) {
        // `file:` is always relative to the root; without a root there is no
        // sandbox to resolve against, so the artifact stays unavailable.
        let root = root?;
        let resolved = read_sandboxed(root, path)?;
        return Some((resolved.1, Some(resolved.0)));
    }

    // A plain path is checked against the sandbox when there is a root, and
    // against the process directory as well, so a pre-W4 caller that passed
    // relative paths keeps working.
    let resolved = root
        .and_then(|root| read_sandboxed(root, reference))
        .or_else(|| read_relative(reference));
    if let Some(resolved) = resolved {
        return Some((resolved.1, Some(resolved.0)));
    }

    Some((reference.to_owned(), None))
}

fn read_relative(candidate: &str) -> Option<(String, String)> {
    if !is_relative_safe(candidate) {
        return None;
    }
    let resolved = read_file(Path::new(candidate))?;
    let display = Path::new(candidate)
        .canonicalize()
        .ok()?
        .display()
        .to_string();
    Some((display, resolved))
}

fn read_sandboxed(root: &Path, candidate: &str) -> Option<(String, String)> {
    if !is_relative_safe(candidate) {
        return None;
    }
    let canonical_root = root.canonicalize().ok()?;
    let joined = canonical_root.join(candidate);
    let canonical = joined.canonicalize().ok()?;
    // Canonicalisation also resolves symlinks, so this rejects a link that
    // points outside the root.
    if !canonical.starts_with(&canonical_root) {
        return None;
    }
    let content = read_file(&canonical)?;
    Some((canonical.display().to_string(), content))
}

fn is_relative_safe(candidate: &str) -> bool {
    if candidate.trim().is_empty() {
        return false;
    }
    let path = Path::new(candidate);
    !path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    })
}

fn read_file(path: &Path) -> Option<String> {
    let metadata = path.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_ARTIFACT_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    String::from_utf8(bytes).ok()
}

/// One independent machine check over a run's artifacts.
///
/// Implementations read the given artifacts only. No model, network or shell
/// access is allowed.
#[async_trait]
pub trait Checker: Send + Sync {
    /// Returns the verdict and the evidence it observed.
    async fn check(
        &self,
        run: &RunSnapshot,
        artifacts: &ArtifactSet,
    ) -> Result<CheckResult, SilynError>;
}

/// How [`CompositeVerifier`] combines individual results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerificationPolicy {
    /// Any explicit failure fails the run; all checks must pass for success;
    /// anything else is `Unknown`.
    #[default]
    AnyFailureFails,
    /// Every configured check must pass. Missing evidence (an `Unknown` check)
    /// fails verification instead of being reported as `Unknown`.
    AllMustSucceed,
}

/// Runs every configured checker and applies one policy.
///
/// There is no short-circuit: every checker runs so the evidence list is
/// complete, then the first failure decides the code and reason.
pub struct CompositeVerifier {
    checkers: Vec<Arc<dyn Checker>>,
    policy: VerificationPolicy,
    root: Option<PathBuf>,
}

impl CompositeVerifier {
    /// Empty checker list, default policy, no artifact root.
    #[must_use]
    pub fn new(checkers: Vec<Arc<dyn Checker>>) -> Self {
        Self {
            checkers,
            policy: VerificationPolicy::default(),
            root: None,
        }
    }

    /// Replaces the combination policy.
    #[must_use]
    pub fn with_policy(mut self, policy: VerificationPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Sets the sandbox root for `file:` artifact references.
    ///
    /// Without a root, `file:` references are unavailable and only inline or
    /// plain references are inspected.
    #[must_use]
    pub fn with_artifact_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.root = Some(root.into());
        self
    }

    /// Number of configured checkers.
    #[must_use]
    pub fn checker_count(&self) -> usize {
        self.checkers.len()
    }

    /// Builds the artifact set for a run with the configured root.
    #[must_use]
    pub fn artifacts(&self, run: &RunSnapshot) -> ArtifactSet {
        ArtifactSet::new(self.root.clone(), &run.artifact_refs)
    }

    /// Combines results without touching the network or the model.
    #[must_use]
    pub fn combine(&self, results: &[CheckResult]) -> CheckResult {
        if results.is_empty() {
            return CheckResult::Unknown {
                reason: "no checkers configured".to_owned(),
            };
        }

        let evidence = merged_evidence(results);
        let first_failure = results.iter().find(|result| result.is_failed());
        let unknown_reasons: Vec<&str> = results
            .iter()
            .filter_map(|result| match result {
                CheckResult::Unknown { reason } => Some(reason.as_str()),
                _ => None,
            })
            .collect();

        if let Some(CheckResult::Failed { code, reason, .. }) = first_failure {
            return CheckResult::Failed {
                code: code.clone(),
                reason: reason.clone(),
                evidence,
            };
        }

        let all_success = results.iter().all(CheckResult::is_success);
        match (self.policy, all_success) {
            (_, true) => CheckResult::Success { evidence },
            (VerificationPolicy::AllMustSucceed, false) => CheckResult::Failed {
                code: "unverified".to_owned(),
                reason: if unknown_reasons.is_empty() {
                    "not every check succeeded".to_owned()
                } else {
                    unknown_reasons.join("; ")
                },
                evidence,
            },
            (VerificationPolicy::AnyFailureFails, false) => CheckResult::Unknown {
                reason: if unknown_reasons.is_empty() {
                    "insufficient evidence".to_owned()
                } else {
                    unknown_reasons.join("; ")
                },
            },
        }
    }
}

#[async_trait]
impl OutcomeVerifier for CompositeVerifier {
    async fn verify(&self, run: RunSnapshot) -> Result<OutcomeEvidence, SilynError> {
        let artifacts = self.artifacts(&run);
        let mut results = Vec::with_capacity(self.checkers.len());
        for checker in &self.checkers {
            results.push(checker.check(&run, &artifacts).await?);
        }
        let result = self.combine(&results);

        let (outcome, confidence) = match &result {
            CheckResult::Success { .. } => (VerifiedOutcome::Success, SUCCESS_CONFIDENCE),
            CheckResult::Failed { .. } => (VerifiedOutcome::Failed, FAILED_CONFIDENCE),
            CheckResult::Unknown { .. } => (VerifiedOutcome::Unknown, UNKNOWN_CONFIDENCE),
        };

        Ok(OutcomeEvidence {
            outcome,
            evidence_refs: result.evidence().to_vec(),
            confidence,
        })
    }
}

fn merged_evidence(results: &[CheckResult]) -> Vec<String> {
    let mut merged: Vec<String> = Vec::new();
    for result in results {
        for item in result.evidence() {
            if !merged.contains(item) {
                merged.push(item.clone());
            }
        }
    }
    merged
}

/// Compares a JSON artifact against an expected answer.
///
/// Objects are subset-matched recursively: every expected key must be present
/// and equal, extra keys are allowed. Arrays and scalars must match exactly.
/// The first artifact that looks like a JSON document is compared; a malformed
/// document is a failure, not a guess.
pub struct ExpectedAnswerChecker {
    expected: serde_json::Value,
}

impl ExpectedAnswerChecker {
    /// Builds a checker for one expected document.
    #[must_use]
    pub fn new(expected: serde_json::Value) -> Self {
        Self { expected }
    }
}

#[async_trait]
impl Checker for ExpectedAnswerChecker {
    async fn check(
        &self,
        _run: &RunSnapshot,
        artifacts: &ArtifactSet,
    ) -> Result<CheckResult, SilynError> {
        for (index, content) in artifacts.all().iter().enumerate() {
            if !looks_like_json(content) {
                continue;
            }

            let parsed: serde_json::Value = match serde_json::from_str(content) {
                Ok(parsed) => parsed,
                Err(error) => {
                    return Ok(CheckResult::Failed {
                        code: "malformed_artifact".to_owned(),
                        reason: format!("artifact {index} is not valid json: {error}"),
                        evidence: vec![format!("artifact:{index}")],
                    });
                }
            };

            return Ok(match first_mismatch(&self.expected, &parsed, "$") {
                None => CheckResult::Success {
                    evidence: vec![format!(
                        "expected_answer:{index}:{} fields",
                        expected_fields(&self.expected)
                    )],
                },
                Some(path) => CheckResult::Failed {
                    code: "wrong_answer".to_owned(),
                    reason: format!("artifact {index} differs from the expected answer at {path}"),
                    evidence: vec![format!("artifact:{index}"), format!("mismatch:{path}")],
                },
            });
        }

        Ok(CheckResult::Unknown {
            reason: "no json artifact to compare".to_owned(),
        })
    }
}

fn looks_like_json(content: &str) -> bool {
    let trimmed = content.trim_start();
    trimmed.starts_with('{') || trimmed.starts_with('[')
}

fn expected_fields(value: &serde_json::Value) -> usize {
    value.as_object().map_or(1, serde_json::Map::len)
}

/// Returns the path of the first mismatch, or `None` when `actual` satisfies
/// `expected`.
fn first_mismatch(
    expected: &serde_json::Value,
    actual: &serde_json::Value,
    path: &str,
) -> Option<String> {
    match (expected, actual) {
        (serde_json::Value::Object(expected), serde_json::Value::Object(actual)) => {
            for (key, expected_value) in expected {
                let Some(actual_value) = actual.get(key) else {
                    return Some(format!("{path}.{key}"));
                };
                if let Some(mismatch) =
                    first_mismatch(expected_value, actual_value, &format!("{path}.{key}"))
                {
                    return Some(mismatch);
                }
            }
            None
        }
        (serde_json::Value::Object(_), _) => Some(path.to_owned()),
        (expected, actual) if expected == actual => None,
        _ => Some(path.to_owned()),
    }
}

/// Fails unless some artifact contains the required text.
pub struct FileContainsChecker {
    needle: String,
}

impl FileContainsChecker {
    /// Builds a checker for one literal needle.
    #[must_use]
    pub fn new(needle: impl Into<String>) -> Self {
        Self {
            needle: needle.into(),
        }
    }
}

#[async_trait]
impl Checker for FileContainsChecker {
    async fn check(
        &self,
        _run: &RunSnapshot,
        artifacts: &ArtifactSet,
    ) -> Result<CheckResult, SilynError> {
        if self.needle.is_empty() {
            // An empty needle would match every artifact, which is a vacuous
            // success. Refuse instead of reporting one.
            return Ok(CheckResult::Failed {
                code: "invalid_checker".to_owned(),
                reason: "the needle must not be empty".to_owned(),
                evidence: Vec::new(),
            });
        }

        if artifacts.is_empty() {
            return Ok(CheckResult::Unknown {
                reason: "no artifacts to inspect".to_owned(),
            });
        }

        for (index, content) in artifacts.all().iter().enumerate() {
            if content.contains(&self.needle) {
                return Ok(CheckResult::Success {
                    evidence: vec![format!("artifact_contains:{index}")],
                });
            }
        }

        Ok(CheckResult::Failed {
            code: "missing_text".to_owned(),
            reason: "no artifact contains the required text".to_owned(),
            evidence: vec![format!("artifacts_inspected:{}", artifacts.len())],
        })
    }
}
