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
        "verify-migrations" => verify_migrations(),
        "hash-migrations" => hash_migrations(),
        "verify-schemas" => verify_schemas(),
        "verify-arch" => verify_arch(),
        "verify-eval-integrity" => verify_eval_integrity(),
        other => {
            eprintln!(
                "unknown command `{other}`\nusage: cargo run -p xtask -- \
                 <verify-acceptance|verify-migrations|hash-migrations|verify-schemas|verify-arch|verify-eval-integrity>"
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
        ("silyn-eval", &["silyn-core"][..]),
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
