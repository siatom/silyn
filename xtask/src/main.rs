//! Repository gates. Run through `cargo run -p xtask -- <command>`.
//!
//! These checks are the automated replacement for human review: they make
//! weakening tests, editing merged migrations, drifting schemas or inverting
//! crate dependencies fail CI.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

fn main() -> ExitCode {
    let command = std::env::args().nth(1).unwrap_or_default();
    let result = match command.as_str() {
        "verify-acceptance" => verify_acceptance(),
        "verify-docs" => verify_docs(),
        "verify-migrations" => verify_migrations(),
        "hash-migrations" => hash_migrations(),
        "verify-schemas" => verify_schemas(),
        "verify-arch" => verify_arch(),
        "verify-eval-integrity" => verify_eval_integrity(),
        other => {
            eprintln!(
                "unknown command `{other}`\nusage: cargo run -p xtask -- \
                 <verify-acceptance|verify-docs|verify-migrations|hash-migrations|verify-schemas|verify-arch|verify-eval-integrity>"
            );
            return ExitCode::FAILURE;
        }
    };

    match result {
        Ok(()) => {
            println!("ok: {command}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives inside the workspace")
        .to_path_buf()
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

/// A shipped document the operator guide depends on.
struct DocSpec {
    path: &'static str,
    sections: &'static [&'static str],
}

/// Headings every required document must contain.
///
/// The gate is deliberately literal: these are the sections an operator needs
/// when something is on fire, and a heading is the cheapest stable anchor.
const REQUIRED_DOCS: &[DocSpec] = &[
    DocSpec {
        path: "docs/security.md",
        sections: &[
            "## Trust boundaries",
            "## Capabilities and default deny",
            "## Cancel file",
            "## Secrets",
            "## Sealed evaluation suite",
            "## Threat model",
            "## Reporting a vulnerability",
        ],
    },
    DocSpec {
        path: "docs/operations.md",
        sections: &[
            "## Backup",
            "## Restore",
            "## Retention and purge",
            "## Policy rollback",
            "## Kill and resume",
            "## Indeterminate tool calls",
            "## Logs",
            "## Local-only assumptions",
        ],
    },
    DocSpec {
        path: "docs/experiments/template.md",
        sections: &[
            "## Identity",
            "## Suite",
            "## Budget",
            "## Per-arm results",
            "## Paired comparison",
            "## Verdict",
            "## Reproduction",
        ],
    },
    DocSpec {
        path: "docs/architecture/observability.md",
        sections: &[
            "## Status",
            "## Span names",
            "## Fields",
            "## Redaction rules",
        ],
    },
];

const README_SECTIONS: &[&str] = &["## Quickstart", "## Security", "## License"];

fn verify_docs() -> Result<()> {
    verify_docs_in(&root())?;
    println!("docs verified");
    Ok(())
}

fn verify_docs_in(root: &Path) -> Result<()> {
    let readme_path = root.join("README.md");
    let readme = fs::read_to_string(&readme_path)
        .with_context(|| format!("read {}", readme_path.display()))?;
    check_markdown_sections(&readme_path, &readme, README_SECTIONS)?;

    for spec in REQUIRED_DOCS {
        let path = root.join(spec.path);
        let text = fs::read_to_string(&path)
            .with_context(|| format!("read required doc {}", path.display()))?;
        check_markdown_sections(&path, &text, spec.sections)?;
    }

    let documents = shipped_docs(root)?;
    for path in &documents {
        let text = fs::read_to_string(path)
            .with_context(|| format!("read shipped doc {}", path.display()))?;
        check_docs_links(root, path, &text)?;
        check_unresolved_placeholders(path, &text)?;
    }

    Ok(())
}

fn check_markdown_sections(path: &Path, text: &str, sections: &[&str]) -> Result<()> {
    if !text.starts_with("# ") {
        bail!("{} must start with a level-1 title", path.display());
    }
    for section in sections {
        if !text.contains(section) {
            bail!("{} is missing required section `{section}`", path.display());
        }
    }
    Ok(())
}

/// Markdown files that ship to users: the README and `docs/**`.
///
/// `docs/agents/tasks/**` holds internal task packages (which legitimately talk
/// about `TODO` as a rule) and `docs.local/**` is never committed, so neither is
/// part of the shipped surface.
fn shipped_docs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut documents = Vec::new();
    if root.join("README.md").is_file() {
        documents.push(root.join("README.md"));
    }
    collect_markdown(&root.join("docs"), &mut documents)?;

    documents.retain(|path| {
        let relative = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        !relative.starts_with("docs/agents/tasks/")
    });
    documents.sort();
    Ok(documents)
}

fn collect_markdown(dir: &Path, documents: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|value| value.to_str());
            if matches!(name, Some("docs.local" | "target" | ".git")) {
                continue;
            }
            collect_markdown(&path, documents)?;
        } else if path.extension().and_then(|value| value.to_str()) == Some("md") {
            documents.push(path);
        }
    }
    Ok(())
}

fn check_docs_links(root: &Path, path: &Path, text: &str) -> Result<()> {
    for target in markdown_link_targets(text) {
        if target.starts_with("docs.local") || target.contains("/docs.local") {
            bail!(
                "{} links to `{target}`; private documents must not ship",
                path.display()
            );
        }
        if is_external_link(&target) {
            continue;
        }

        let path_part = target
            .split('#')
            .next()
            .unwrap_or_default()
            .split('?')
            .next()
            .unwrap_or_default();
        if path_part.is_empty() {
            continue;
        }

        let resolved = if let Some(stripped) = path_part.strip_prefix('/') {
            root.join(stripped)
        } else {
            path.parent().unwrap_or(root).join(path_part)
        };
        if !resolved.exists() {
            bail!(
                "{} has a broken relative link `{target}` (resolved to {})",
                path.display(),
                resolved.display()
            );
        }
    }
    Ok(())
}

fn is_external_link(target: &str) -> bool {
    target.starts_with('#')
        || target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("mailto:")
}

/// Extracts `[text](target)` and `![alt](target)` targets, ignoring fenced and
/// inline code so command examples cannot trip the link gate.
fn markdown_link_targets(text: &str) -> Vec<String> {
    let text = strip_markdown_code(text);
    let mut targets = Vec::new();
    let mut cursor = 0_usize;

    while let Some(offset) = text[cursor..].find("](") {
        let start = cursor + offset + 2;
        let Some(end_offset) = text[start..].find(')') else {
            break;
        };
        let end = start + end_offset;
        let raw = text[start..end].trim();
        let raw = raw
            .strip_prefix('<')
            .and_then(|value| value.strip_suffix('>'))
            .unwrap_or(raw);
        let target = raw.split_whitespace().next().unwrap_or_default();
        if !target.is_empty() {
            targets.push(target.to_owned());
        }
        cursor = end + 1;
    }

    targets
}

fn strip_markdown_code(text: &str) -> String {
    let mut stripped = String::new();
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        for (index, chunk) in line.split('`').enumerate() {
            if index % 2 == 0 {
                stripped.push_str(chunk);
            }
        }
        stripped.push('\n');
    }
    stripped
}

fn check_unresolved_placeholders(path: &Path, text: &str) -> Result<()> {
    // Inline code often describes the rule itself (“no unresolved `TODO`”), so
    // inline spans are ignored. Fenced examples are still checked: a marker left
    // in a command or template is unresolved.
    let prose = strip_inline_code(text);
    for marker in ["TODO", "TBD"] {
        if contains_word(&prose, marker) {
            bail!(
                "{} contains an unresolved `{marker}` marker; ship a decision instead",
                path.display()
            );
        }
    }
    Ok(())
}

/// Removes inline code spans while keeping fenced blocks and their content.
fn strip_inline_code(text: &str) -> String {
    let mut stripped = String::new();
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            stripped.push('\n');
            continue;
        }
        if in_fence {
            stripped.push_str(line);
            stripped.push('\n');
            continue;
        }
        for (index, chunk) in line.split('`').enumerate() {
            if index % 2 == 0 {
                stripped.push_str(chunk);
            }
        }
        stripped.push('\n');
    }
    stripped
}

fn contains_word(text: &str, word: &str) -> bool {
    text.match_indices(word).any(|(index, _)| {
        let before = text[..index].chars().next_back();
        let after = text[index + word.len()..].chars().next();
        let boundary = |value: Option<char>| {
            !value.is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
        };
        boundary(before) && boundary(after)
    })
}

fn verify_acceptance() -> Result<()> {
    let dir = root().join("eval/acceptance");
    let mut files = Vec::new();
    for entry in fs::read_dir(&dir).context("read eval/acceptance")? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) == Some("toml") {
            files.push(path);
        }
    }
    files.sort();

    let mut seen_ids = std::collections::BTreeSet::new();
    let mut total = 0_usize;

    for file in files {
        let text = fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
        let value: toml::Value =
            toml::from_str(&text).with_context(|| format!("parse {}", file.display()))?;
        let Some(gates) = value.get("gate").and_then(toml::Value::as_array) else {
            continue;
        };

        for gate in gates {
            let id = gate
                .get("id")
                .and_then(toml::Value::as_str)
                .context("gate.id must be a string")?;
            if !seen_ids.insert(id.to_owned()) {
                bail!("duplicate gate id `{id}`");
            }
            let test = gate
                .get("test")
                .and_then(toml::Value::as_str)
                .context("gate.test must be a string")?;
            let required = gate
                .get("required")
                .and_then(toml::Value::as_bool)
                .unwrap_or(true);

            let (source_file, name) = test
                .split_once("::")
                .with_context(|| format!("gate `{id}` test must be `<path>::<fn>`"))?;
            let path = root().join(source_file);
            let source = fs::read_to_string(&path).with_context(|| {
                format!("gate `{id}` references missing file {}", path.display())
            })?;
            let needle = format!("fn {name}");
            let index = source.find(&needle).with_context(|| {
                format!("gate `{id}` references missing test `{name}` in {source_file}")
            })?;

            if required {
                let start = source[..index]
                    .char_indices()
                    .rev()
                    .take(200)
                    .last()
                    .map_or(0, |(offset, _)| offset);
                if source[start..index].contains("#[ignore") {
                    bail!("gate `{id}` test `{name}` is #[ignore]d but marked required");
                }
            }
            total += 1;
        }
    }

    if total == 0 {
        bail!("no acceptance gates found");
    }
    println!("acceptance gates: {total}");
    Ok(())
}

fn verify_migrations() -> Result<()> {
    let dir = root().join("migrations");
    let checksum_path = dir.join("checksums.txt");
    let text = fs::read_to_string(&checksum_path).with_context(|| {
        format!(
            "read {} (run `cargo run -p xtask -- hash-migrations` first)",
            checksum_path.display()
        )
    })?;

    let mut expected = BTreeMap::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let (hash, name) = line
            .split_once("  ")
            .with_context(|| format!("malformed checksums line: {line}"))?;
        expected.insert(name.to_owned(), hash.to_owned());
    }

    let mut seen = 0_usize;
    for entry in fs::read_dir(&dir).context("read migrations directory")? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("sql") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .context("migration filename must be utf-8")?
            .to_owned();
        let hash = sha256_file(&path)?;
        match expected.get(&name) {
            Some(want) if want == &hash => seen += 1,
            Some(_) => bail!(
                "migration `{name}` was modified after merge; add a new migration instead of editing it"
            ),
            None => bail!("migration `{name}` is not listed in migrations/checksums.txt"),
        }
    }

    if seen != expected.len() {
        bail!(
            "migrations/checksums.txt lists {} files but only {seen} were found",
            expected.len()
        );
    }

    println!("migrations verified: {seen}");
    Ok(())
}

fn hash_migrations() -> Result<()> {
    let dir = root().join("migrations");
    let mut lines = Vec::new();
    for entry in fs::read_dir(&dir).context("read migrations directory")? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("sql") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .context("migration filename must be utf-8")?
            .to_owned();
        lines.push(format!("{}  {name}", sha256_file(&path)?));
    }
    if lines.is_empty() {
        bail!("no .sql migrations found");
    }
    lines.sort();
    fs::write(dir.join("checksums.txt"), format!("{}\n", lines.join("\n")))
        .context("write migrations/checksums.txt")?;
    println!("wrote {} migration checksums", lines.len());
    Ok(())
}

fn verify_schemas() -> Result<()> {
    let dir = root().join("schemas");
    let mut count = 0_usize;
    for entry in fs::read_dir(&dir).context("read schemas directory")? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let value: serde_json::Value =
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        let object = value
            .as_object()
            .with_context(|| format!("{} must contain a JSON object", path.display()))?;
        for required in ["$schema", "title", "type"] {
            if !object.contains_key(required) {
                bail!("{} is missing `{required}`", path.display());
            }
        }
        count += 1;
    }
    if count == 0 {
        bail!("no schemas found");
    }
    println!("schemas verified: {count}");
    Ok(())
}

fn verify_arch() -> Result<()> {
    let allowed: BTreeMap<&str, &[&str]> = BTreeMap::from([
        ("silyn-core", &[][..]),
        ("silyn-storage", &["silyn-core"][..]),
        ("silyn-llm", &["silyn-core"][..]),
        (
            "silyn-runtime",
            &["silyn-core", "silyn-llm", "silyn-storage"][..],
        ),
        ("silyn-growth", &["silyn-core", "silyn-storage"][..]),
        (
            "silyn-app",
            &[
                "silyn-core",
                "silyn-growth",
                "silyn-llm",
                "silyn-runtime",
                "silyn-storage",
            ][..],
        ),
        ("silyn-cli", &["silyn-app", "silyn-core", "silyn-llm"][..]),
        ("silyn-eval", &["silyn-core", "silyn-llm"][..]),
    ]);

    let crates_dir = root().join("crates");
    for entry in fs::read_dir(&crates_dir).context("read crates directory")? {
        let dir = entry?.path();
        if !dir.is_dir() {
            continue;
        }
        let manifest_path = dir.join("Cargo.toml");
        let text = fs::read_to_string(&manifest_path)
            .with_context(|| format!("read {}", manifest_path.display()))?;
        let value: toml::Value = toml::from_str(&text).context("parse Cargo.toml")?;
        let name = value
            .get("package")
            .and_then(|package| package.get("name"))
            .and_then(toml::Value::as_str)
            .context("package.name")?;
        let Some(permitted) = allowed.get(name) else {
            continue;
        };
        let Some(dependencies) = value.get("dependencies").and_then(toml::Value::as_table) else {
            continue;
        };
        for dependency in dependencies.keys() {
            if !dependency.starts_with("silyn-") {
                continue;
            }
            if !permitted.contains(&dependency.as_str()) {
                bail!("crate `{name}` must not depend on `{dependency}` (allowed: {permitted:?})");
            }
        }
    }

    println!("crate dependency directions verified");
    Ok(())
}

fn verify_eval_integrity() -> Result<()> {
    let suites_dir = root().join("eval/suites");
    if !suites_dir.exists() {
        println!("no suites directory yet");
        return Ok(());
    }

    let mut locked = 0_usize;
    for entry in fs::read_dir(&suites_dir).context("read eval/suites")? {
        let suite_dir = entry?.path();
        if !suite_dir.is_dir() {
            continue;
        }
        let lock_path = suite_dir.join("suite.lock");
        if !lock_path.exists() {
            continue;
        }
        locked += 1;
        let lock = fs::read_to_string(&lock_path)
            .with_context(|| format!("read {}", lock_path.display()))?;
        for line in lock.lines().filter(|line| !line.trim().is_empty()) {
            let (hash, relative) = line
                .split_once("  ")
                .with_context(|| format!("malformed suite.lock line: {line}"))?;
            let file = suite_dir.join(relative);
            let actual = sha256_file(&file)
                .with_context(|| format!("suite artifact missing: {}", file.display()))?;
            if actual != hash {
                bail!(
                    "suite artifact `{}` changed after sealing; create a new suite version",
                    file.display()
                );
            }
        }
    }

    println!("sealed suites verified: {locked}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    struct TempWorkspace {
        path: PathBuf,
    }

    impl TempWorkspace {
        fn new(name: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "silyn-xtask-{name}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            if path.exists() {
                fs::remove_dir_all(&path).expect("clean temp workspace");
            }
            fs::create_dir_all(&path).expect("create temp workspace");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.path.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("create fixture directory");
            }
            fs::write(path, text).expect("write fixture");
        }
    }

    impl Drop for TempWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn write_valid_docs(workspace: &TempWorkspace) {
        workspace.write(
            "README.md",
            "# Fixture\n\n## Quickstart\n\n[security](docs/security.md)\n\n\
             ## Security\n\nMIT licensed.\n\n## License\n\nMIT.\n",
        );
        workspace.write(
            "docs/security.md",
            "# Security\n\n## Trust boundaries\n\n## Capabilities and default deny\n\n\
             ## Cancel file\n\n## Secrets\n\n## Sealed evaluation suite\n\n\
             ## Threat model\n\n## Reporting a vulnerability\n",
        );
        workspace.write(
            "docs/operations.md",
            "# Operations\n\n## Backup\n\n## Restore\n\n## Retention and purge\n\n\
             ## Policy rollback\n\n## Kill and resume\n\n## Indeterminate tool calls\n\n\
             ## Logs\n\n## Local-only assumptions\n",
        );
        workspace.write(
            "docs/experiments/template.md",
            "# Experiment template\n\n## Identity\n\n## Suite\n\n## Budget\n\n\
             ## Per-arm results\n\n## Paired comparison\n\n## Verdict\n\n## Reproduction\n",
        );
        workspace.write(
            "docs/architecture/observability.md",
            "# Observability\n\n## Status\n\n## Span names\n\n## Fields\n\n\
             ## Redaction rules\n",
        );
    }

    fn check_real_doc(path: &str) {
        let root = root();
        let spec = REQUIRED_DOCS
            .iter()
            .find(|spec| spec.path == path)
            .expect("doc is registered in REQUIRED_DOCS");
        let full = root.join(path);
        let text = fs::read_to_string(&full).expect("read shipped doc");
        check_markdown_sections(&full, &text, spec.sections).expect("sections present");
    }

    #[test]
    fn docs_gate_accepts_repository() {
        verify_docs_in(&root()).expect("repository docs satisfy the gate");
    }

    #[test]
    fn readme_has_quickstart_security_and_license() {
        let path = root().join("README.md");
        let text = fs::read_to_string(&path).expect("read README");
        check_markdown_sections(&path, &text, README_SECTIONS).expect("README sections present");
    }

    #[test]
    fn security_doc_covers_trust_boundaries() {
        check_real_doc("docs/security.md");
    }

    #[test]
    fn operations_doc_covers_recovery() {
        check_real_doc("docs/operations.md");
    }

    #[test]
    fn experiment_template_records_protocol() {
        check_real_doc("docs/experiments/template.md");
    }

    #[test]
    fn observability_doc_defines_span_contract() {
        check_real_doc("docs/architecture/observability.md");
    }

    #[test]
    fn docs_gate_rejects_missing_section() {
        let workspace = TempWorkspace::new("missing-section");
        write_valid_docs(&workspace);
        workspace.write(
            "README.md",
            "# Fixture\n\n## Quickstart\n\n## Security\n\nNo license heading.\n",
        );

        let error = verify_docs_in(workspace.path()).expect_err("missing section must fail");
        assert!(
            format!("{error:#}").contains("missing required section `## License`"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn docs_gate_rejects_docs_local_link() {
        let workspace = TempWorkspace::new("docs-local");
        write_valid_docs(&workspace);
        let readme = format!(
            "{}\n[private](docs.local/plan.md)\n",
            fs::read_to_string(workspace.path().join("README.md")).expect("read fixture README")
        );
        workspace.write("README.md", &readme);

        let error = verify_docs_in(workspace.path()).expect_err("docs.local link must fail");
        assert!(
            format!("{error:#}").contains("docs.local"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn docs_gate_rejects_unresolved_todo() {
        let workspace = TempWorkspace::new("todo");
        write_valid_docs(&workspace);
        let security = format!(
            "{}\nTODO: describe the remaining boundary.\n",
            fs::read_to_string(workspace.path().join("docs/security.md")).expect("read fixture")
        );
        workspace.write("docs/security.md", &security);

        let error = verify_docs_in(workspace.path()).expect_err("TODO must fail");
        assert!(
            format!("{error:#}").contains("unresolved `TODO`"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn docs_gate_rejects_broken_relative_link() {
        let workspace = TempWorkspace::new("broken-link");
        write_valid_docs(&workspace);
        let readme = format!(
            "{}\n[missing](docs/does-not-exist.md)\n",
            fs::read_to_string(workspace.path().join("README.md")).expect("read fixture README")
        );
        workspace.write("README.md", &readme);

        let error = verify_docs_in(workspace.path()).expect_err("broken link must fail");
        assert!(
            format!("{error:#}").contains("broken relative link"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn ci_workflow_has_three_platforms() {
        let path = root().join(".github/workflows/ci.yml");
        let workflow = fs::read_to_string(&path).expect("read CI workflow");
        for runner in ["windows-latest", "ubuntu-latest", "macos-latest"] {
            assert!(workflow.contains(runner), "CI matrix is missing {runner}");
        }
        assert!(workflow.contains("1.99.0"), "CI must pin Rust 1.99");
    }

    #[test]
    fn ci_workflow_runs_all_gates() {
        let path = root().join(".github/workflows/ci.yml");
        let workflow = fs::read_to_string(&path).expect("read CI workflow");
        for command in [
            "cargo fmt --all --check",
            "cargo clippy --workspace --all-targets -- -D warnings",
            "cargo test --workspace",
            "cargo build --workspace --release",
            "cargo run -p xtask -- verify-docs",
            "cargo run -p xtask -- verify-acceptance",
            "cargo run -p xtask -- verify-migrations",
            "cargo run -p xtask -- verify-schemas",
            "cargo run -p xtask -- verify-arch",
            "cargo run -p xtask -- verify-eval-integrity",
        ] {
            assert!(workflow.contains(command), "CI is missing `{command}`");
        }
        assert!(
            !workflow.contains("secrets."),
            "CI must not consume repository secrets"
        );
    }
}
