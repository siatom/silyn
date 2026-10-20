//! Deterministic generator for the `csv-stats-v1` task family.
//!
//! Every task is derived from a single seed through a splitmix64 stream; no
//! external RNG crate is involved. The oracle is written from the row model
//! *before* the CSV is rendered, so it never depends on parsing the text under
//! test. Same seed => byte-identical CSV and identical oracle.
//!
//! Traps injected per task: unit mismatch (header vs values), empty cells,
//! duplicate rows, thousands separators, column-name drift and an optional
//! UTF-8 byte order mark. Timestamps always carry a timezone offset.

use sha2::{Digest, Sha256};
use silyn_core::SilynError;

use crate::rng::SplitMix64;
use crate::task::{Oracle, Split, Task, TaskInput, TaskMetadata, TrapFlags, Unit};

/// Suite id shared with `eval/suites/csv-stats-v1/suite.toml`.
pub const SUITE_ID: &str = "csv-stats-v1";

/// Task family id recorded in the suite manifest.
pub const TASK_FAMILY: &str = "csv-stats";

/// Objective shown to every candidate. It defines the counting rules exactly.
pub const OBJECTIVE: &str = concat!(
    "Analyse the attached CSV (columns: id, mass, quantity, timestamp; names and ",
    "units drift between files). Reply with a JSON object: ",
    "{\"row_count\": distinct data rows (exact duplicate copies counted once), ",
    "\"missing_total\": empty cells across all data rows as written (duplicate ",
    "copies included), \"duplicate_rows\": rows that duplicate an earlier row, ",
    "\"unit\": actual unit of the mass column, \"kg\" or \"lb\", inferred from the ",
    "values; the header may be wrong}."
);

const TRAIN_BASE_SEED: u64 = 0x0000_0001_0000_0000;
const DEV_BASE_SEED: u64 = 0x0000_0002_0000_0000;
const TEST_BASE_SEED: u64 = 0x0000_0003_0000_0000;
const SEED_STRIDE: u64 = 0x9E37_79B9_7F4A_7C15;
const KG_TO_LB: f64 = 2.204_622_621_848_137_6;

const MASS_NAMES_KG: [&str; 4] = ["mass_kg", "weight_kg", "Mass (kg)", "MASS_KG"];
const MASS_NAMES_LB: [&str; 4] = ["mass_lb", "weight_lb", "Mass (lb)", "MASS_LB"];
const QUANTITY_NAMES: [&str; 4] = ["quantity", "qty", "units_sold", "Quantity"];
const TIMESTAMP_NAMES: [&str; 4] = ["timestamp", "recorded_at", "ts", "Recorded At"];
const OFFSETS: [&str; 4] = ["Z", "+02:00", "-05:00", "+05:30"];

/// Seed for a task. Stable across platforms and releases of this crate.
#[must_use]
pub fn task_seed(split: Split, index: u32) -> u64 {
    let base = match split {
        Split::Train => TRAIN_BASE_SEED,
        Split::Dev => DEV_BASE_SEED,
        Split::Test => TEST_BASE_SEED,
    };
    base.wrapping_add(u64::from(index).wrapping_add(1).wrapping_mul(SEED_STRIDE))
}

/// Generates one task. `index` must be below [`Split::count`].
#[must_use]
pub fn generate_task(split: Split, index: u32) -> Task {
    assert!(
        index < split.count(),
        "task index {index} out of range for split {split}"
    );
    let seed = task_seed(split, index);
    let mut rng = SplitMix64::new(seed);

    let unit = if rng.below(2) == 0 {
        Unit::Kg
    } else {
        Unit::Lb
    };
    let unit_mismatch = rng.below(2) == 0;
    let unique_rows = 6 + rng.below(7) as u32;
    let duplicate_rows = 1 + rng.below(2) as u32;
    let thousands_separators = rng.below(2) == 0;
    let byte_order_mark = rng.below(4) == 0;

    let header_unit = if unit_mismatch {
        match unit {
            Unit::Kg => Unit::Lb,
            Unit::Lb => Unit::Kg,
        }
    } else {
        unit
    };
    let mass_name = rng.pick(mass_names(header_unit));
    let quantity_name = rng.pick(&QUANTITY_NAMES);
    let timestamp_name = rng.pick(&TIMESTAMP_NAMES);

    let mut rows = Vec::with_capacity(unique_rows as usize);
    for id in 1..=unique_rows {
        let mass_kg = 40.0 + f64::from(rng.below(800) as u32) / 10.0;
        let mass = match unit {
            Unit::Kg => round1(mass_kg),
            Unit::Lb => round1(mass_kg * KG_TO_LB),
        };
        rows.push(Row {
            id,
            mass: Some(mass),
            quantity: Some(50 + rng.below(11_950) as u32),
            timestamp: Some(timestamp(&mut rng)),
        });
    }

    inject_missing(&mut rows, 1 + rng.below(3), &mut rng);

    let mut candidates: Vec<usize> = (0..rows.len()).collect();
    rng.shuffle(&mut candidates);
    let duplicated: Vec<usize> = candidates
        .into_iter()
        .take(duplicate_rows as usize)
        .collect();

    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.extend(duplicated.iter().copied());
    rng.shuffle(&mut order);

    // Oracle from the generator model, before rendering.
    let missing_total: u32 = order.iter().map(|&index| missing_cells(&rows[index])).sum();
    let oracle = Oracle {
        row_count: unique_rows,
        missing_total,
        duplicate_rows,
        unit,
    };

    let header = format!("id,{mass_name},{quantity_name},{timestamp_name}");
    let csv = render(
        &header,
        &rows,
        &order,
        thousands_separators,
        byte_order_mark,
    );

    let traps = TrapFlags {
        unit_mismatch,
        missing_values: missing_total > 0,
        duplicate_rows: true,
        thousands_separators,
        // Names always drift from the canonical `id,mass_kg,quantity,timestamp`.
        column_name_drift: true,
        byte_order_mark,
    };

    Task {
        metadata: TaskMetadata {
            task_id: format!("{SUITE_ID}/{}/{index:03}", split.as_str()),
            seed,
            split,
            unique_rows,
            emitted_rows: order.len() as u32,
            unit,
            missing_total,
            duplicate_rows,
            traps,
            oracle,
        },
        input: TaskInput {
            objective: OBJECTIVE.to_owned(),
            csv,
        },
    }
}

/// Generates a train or dev split. The test split is denied here on purpose:
/// it is only reachable through [`crate::SealedTestSet`].
pub fn generate_split(split: Split) -> Result<Vec<Task>, SilynError> {
    if split.is_sealed() {
        return Err(SilynError::PolicyDenied(
            "the test split is sealed; evaluate it through SealedTestSet".to_owned(),
        ));
    }
    Ok((0..split.count())
        .map(|index| generate_task(split, index))
        .collect())
}

/// Sealed-split generation. Crate-private so no candidate can call it.
pub(crate) fn sealed_test_tasks() -> Vec<Task> {
    (0..Split::Test.count())
        .map(|index| generate_task(Split::Test, index))
        .collect()
}

/// Digest recorded in `suite.toml` for a split. Works for train, dev and test.
pub fn split_sha256(split: Split) -> std::result::Result<String, SilynError> {
    let tasks = if split.is_sealed() {
        sealed_test_tasks()
    } else {
        generate_split(split)?
    };
    Ok(digest_of(&tasks))
}

/// Canonical digest over task ids, inputs and oracles (length-prefixed fields).
pub(crate) fn digest_of(tasks: &[Task]) -> String {
    let mut hasher = Sha256::new();
    hasher.update((tasks.len() as u64).to_le_bytes());
    for task in tasks {
        let input = serde_json::to_vec(&task.input).expect("TaskInput is always serializable");
        let oracle = serde_json::to_vec(&task.metadata.oracle).expect("Oracle is serializable");
        hasher.update((task.metadata.task_id.len() as u64).to_le_bytes());
        hasher.update(task.metadata.task_id.as_bytes());
        hasher.update((input.len() as u64).to_le_bytes());
        hasher.update(&input);
        hasher.update((oracle.len() as u64).to_le_bytes());
        hasher.update(&oracle);
    }
    format!("{:x}", hasher.finalize())
}

/// Fixed, hand-checkable example used by the README and the oracle test.
///
/// Four distinct rows, two duplicate copies, four empty cells, actual unit `kg`
/// while the header says `lb`, and thousands separators in `quantity`.
#[must_use]
pub fn documented_example() -> Task {
    let rows = vec![
        Row {
            id: 1,
            mass: Some(72.4),
            quantity: Some(1200),
            timestamp: Some("2026-03-01T09:30:00+02:00".to_owned()),
        },
        Row {
            id: 2,
            mass: None,
            quantity: Some(850),
            timestamp: Some("2026-03-01T08:00:00Z".to_owned()),
        },
        Row {
            id: 3,
            mass: Some(95.0),
            quantity: None,
            timestamp: Some("2026-03-01T03:15:00-05:00".to_owned()),
        },
        Row {
            id: 4,
            mass: Some(60.3),
            quantity: Some(4300),
            timestamp: None,
        },
    ];
    let order = vec![0, 1, 2, 3, 0, 2];
    let missing_total = order.iter().map(|&index| missing_cells(&rows[index])).sum();
    let oracle = Oracle {
        row_count: 4,
        missing_total,
        duplicate_rows: 2,
        unit: Unit::Kg,
    };
    let header = "id,mass_lb,quantity,timestamp";
    Task {
        metadata: TaskMetadata {
            task_id: format!("{SUITE_ID}/example"),
            seed: 0,
            split: Split::Train,
            unique_rows: 4,
            emitted_rows: order.len() as u32,
            unit: Unit::Kg,
            missing_total,
            duplicate_rows: 2,
            traps: TrapFlags {
                unit_mismatch: true,
                missing_values: true,
                duplicate_rows: true,
                thousands_separators: true,
                column_name_drift: true,
                byte_order_mark: false,
            },
            oracle,
        },
        input: TaskInput {
            objective: OBJECTIVE.to_owned(),
            csv: render(header, &rows, &order, true, false),
        },
    }
}

#[derive(Debug, Clone)]
struct Row {
    id: u32,
    mass: Option<f64>,
    quantity: Option<u32>,
    timestamp: Option<String>,
}

fn missing_cells(row: &Row) -> u32 {
    (row.mass.is_none() as u32) + (row.quantity.is_none() as u32) + (row.timestamp.is_none() as u32)
}

fn mass_names(unit: Unit) -> &'static [&'static str; 4] {
    match unit {
        Unit::Kg => &MASS_NAMES_KG,
        Unit::Lb => &MASS_NAMES_LB,
    }
}

fn timestamp(rng: &mut SplitMix64) -> String {
    let day = 1 + rng.below(28);
    let hour = rng.below(24);
    let minute = rng.below(60);
    let offset = rng.pick(&OFFSETS);
    format!("2026-03-{day:02}T{hour:02}:{minute:02}:00{offset}")
}

fn inject_missing(rows: &mut [Row], target: u64, rng: &mut SplitMix64) {
    let mut injected = 0;
    let mut attempts = 0;
    while injected < target && attempts < 100 {
        attempts += 1;
        let row = rng.below(rows.len() as u64) as usize;
        let column = rng.below(3);
        let already_missing = match column {
            0 => rows[row].mass.is_none(),
            1 => rows[row].quantity.is_none(),
            _ => rows[row].timestamp.is_none(),
        };
        if already_missing {
            continue;
        }
        match column {
            0 => rows[row].mass = None,
            1 => rows[row].quantity = None,
            _ => rows[row].timestamp = None,
        }
        injected += 1;
    }
}

fn render(
    header: &str,
    rows: &[Row],
    order: &[usize],
    thousands_separators: bool,
    byte_order_mark: bool,
) -> String {
    let mut out = String::new();
    if byte_order_mark {
        out.push('\u{feff}');
    }
    out.push_str(header);
    out.push('\n');
    for &index in order {
        let row = &rows[index];
        let mass = row
            .mass
            .map(|value| format!("{value:.1}"))
            .unwrap_or_default();
        let quantity = row
            .quantity
            .map(|value| {
                if thousands_separators && value >= 1000 {
                    format_thousands(value)
                } else {
                    value.to_string()
                }
            })
            .unwrap_or_default();
        let timestamp = row.timestamp.clone().unwrap_or_default();
        let fields = [row.id.to_string(), mass, quantity, timestamp];
        let line: Vec<String> = fields.iter().map(|field| escape_field(field)).collect();
        out.push_str(&line.join(","));
        out.push('\n');
    }
    out
}

fn format_thousands(value: u32) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn escape_field(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}
