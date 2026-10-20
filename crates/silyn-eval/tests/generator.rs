//! Generator and oracle contract tests.
//!
//! These tests re-derive the ground truth from the rendered CSV with an
//! independent parser; nothing here calls the generator's own oracle logic.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use silyn_eval::generator::{documented_example, generate_split, split_sha256};
use silyn_eval::manifest::SuiteManifest;
use silyn_eval::task::{Split, Unit};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("resolve repository root")
}

fn manifest() -> SuiteManifest {
    let path = repo_root().join("eval/suites/csv-stats-v1/suite.toml");
    let manifest = SuiteManifest::load(&path).expect("load suite manifest");
    manifest.validate().expect("valid suite manifest");
    manifest
}

#[test]
fn oracle_matches_generator_metadata() {
    // 1. Hand-checked fixture: 4 distinct rows, 2 duplicate copies, 4 empty
    // cells (row 2 mass, row 3 quantity, row 4 timestamp and the row 3 copy),
    // actual unit kg while the header says lb.
    let example = documented_example();
    assert_eq!(example.metadata.oracle.row_count, 4);
    assert_eq!(example.metadata.oracle.missing_total, 4);
    assert_eq!(example.metadata.oracle.duplicate_rows, 2);
    assert_eq!(example.metadata.oracle.unit, Unit::Kg);
    assert_eq!(example.metadata.unit, Unit::Kg);
    assert_eq!(example.metadata.unique_rows, 4);
    assert_eq!(example.metadata.emitted_rows, 6);

    let expected_csv = concat!(
        "id,mass_lb,quantity,timestamp\n",
        "1,72.4,\"1,200\",2026-03-01T09:30:00+02:00\n",
        "2,,850,2026-03-01T08:00:00Z\n",
        "3,95.0,,2026-03-01T03:15:00-05:00\n",
        "4,60.3,\"4,300\",\n",
        "1,72.4,\"1,200\",2026-03-01T09:30:00+02:00\n",
        "3,95.0,,2026-03-01T03:15:00-05:00\n",
    );
    assert_eq!(example.input.csv, expected_csv);

    // 2. Every train/dev task: recount from the CSV independently.
    for split in [Split::Train, Split::Dev] {
        let tasks = generate_split(split).expect("generated split");
        for task in tasks {
            let (header, rows) = parse_csv(&task.input.csv);
            let distinct: HashSet<Vec<String>> = rows.iter().cloned().collect();
            let missing_total: u32 = rows
                .iter()
                .flat_map(|row| row.iter())
                .filter(|field| field.is_empty())
                .count() as u32;

            assert_eq!(
                distinct.len() as u32,
                task.metadata.oracle.row_count,
                "{} row_count",
                task.metadata.task_id
            );
            assert_eq!(
                rows.len() as u32 - distinct.len() as u32,
                task.metadata.oracle.duplicate_rows,
                "{} duplicate_rows",
                task.metadata.task_id
            );
            assert_eq!(
                missing_total, task.metadata.oracle.missing_total,
                "{} missing_total",
                task.metadata.task_id
            );
            assert_eq!(
                rows.len() as u32,
                task.metadata.emitted_rows,
                "{} emitted_rows",
                task.metadata.task_id
            );

            let mass_header = header[1].to_ascii_lowercase();
            let header_unit = if mass_header.contains("lb") {
                Unit::Lb
            } else {
                assert!(
                    mass_header.contains("kg"),
                    "{} mass header has no unit token: {}",
                    task.metadata.task_id,
                    header[1]
                );
                Unit::Kg
            };
            assert_eq!(
                header_unit != task.metadata.unit,
                task.metadata.traps.unit_mismatch,
                "{} unit_mismatch flag",
                task.metadata.task_id
            );

            for row in &rows {
                let Some(raw) = row.get(1).filter(|value| !value.is_empty()) else {
                    continue;
                };
                let value: f64 = raw.replace(',', "").parse().expect("mass is numeric");
                match task.metadata.unit {
                    Unit::Kg => assert!(
                        (40.0..=120.0).contains(&value),
                        "{} kg value out of range: {value}",
                        task.metadata.task_id
                    ),
                    Unit::Lb => assert!(
                        (88.0..=265.0).contains(&value),
                        "{} lb value out of range: {value}",
                        task.metadata.task_id
                    ),
                }
            }
        }
    }

    // 3. Trap coverage in the train split is deterministic, not accidental.
    let train = generate_split(Split::Train).expect("train split");
    assert!(train.iter().all(|task| task.metadata.missing_total >= 1));
    assert!(train.iter().all(|task| task.metadata.duplicate_rows >= 1));
    assert!(train.iter().any(|task| task.metadata.traps.unit_mismatch));
    assert!(
        train
            .iter()
            .any(|task| task.metadata.traps.thousands_separators)
    );
    assert!(train.iter().any(|task| task.metadata.traps.byte_order_mark));
}

#[test]
fn split_hashes_are_stable() {
    let manifest = manifest();
    for split in Split::ALL {
        let entry = manifest.split_entry(split);
        assert_eq!(entry.count, split.count(), "{split} count");

        let first = split_sha256(split).expect("split digest");
        let second = split_sha256(split).expect("split digest");
        assert_eq!(first, second, "{split} digest is not stable");
        assert_eq!(
            first, entry.sha256,
            "suite.toml {split} digest is stale; run `cargo run -p silyn-eval -- hashes`"
        );
    }

    for split in [Split::Train, Split::Dev] {
        let first = generate_split(split).expect("split");
        let second = generate_split(split).expect("split");
        assert_eq!(first, second, "{split} tasks differ across runs");
        assert_eq!(first.len(), split.count() as usize, "{split} count");
    }
}

#[test]
fn tasks_differ_across_splits() {
    let train = generate_split(Split::Train).expect("train split");
    let dev = generate_split(Split::Dev).expect("dev split");
    let ids: HashSet<&str> = train
        .iter()
        .chain(dev.iter())
        .map(|task| task.metadata.task_id.as_str())
        .collect();
    assert_eq!(ids.len(), 30);
    assert_ne!(train[0].metadata.seed, dev[0].metadata.seed);
    assert_ne!(train[0].input.csv, dev[0].input.csv);
}

/// Minimal RFC 4180 reader for the generator's subset: comma separated, fields
/// quoted only when they contain a comma, one row per line, optional BOM.
fn parse_csv(text: &str) -> (Vec<String>, Vec<Vec<String>>) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.split('\n');
    let header = parse_line(lines.next().expect("header line"));
    let rows = lines
        .filter(|line| !line.is_empty())
        .map(parse_line)
        .collect();
    (header, rows)
}

fn parse_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' if quoted => {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    current.push('"');
                } else {
                    quoted = false;
                }
            }
            '"' if current.is_empty() => quoted = true,
            ',' if !quoted => fields.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    fields.push(current);
    fields
}
