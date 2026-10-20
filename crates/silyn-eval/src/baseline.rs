//! Honest, memory-free baseline solver (arm A reference).
//!
//! It performs mechanical CSV analysis: distinct rows, duplicate copies and
//! empty cells are counted correctly. It trusts the header for the unit, so it
//! fails every unit-mismatch task, and it does no repair or retry. It never
//! fabricates a success: unparsable input is returned as an error.

use std::collections::HashSet;

use silyn_core::SilynError;

use crate::evaluator::Solver;
use crate::task::{Submission, TaskInput, Unit};

#[derive(Debug, Clone, Copy, Default)]
pub struct BaselineSolver;

impl Solver for BaselineSolver {
    fn solve(&self, input: &TaskInput) -> Result<Submission, SilynError> {
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_reader(input.csv.as_bytes());
        let headers = reader.headers().map_err(csv_error)?.clone();
        let mass_index = headers
            .iter()
            .position(|name| {
                let lower = name.to_ascii_lowercase();
                lower.contains("mass") || lower.contains("weight")
            })
            .ok_or_else(|| SilynError::Permanent("mass column not found".to_owned()))?;

        let mut seen: HashSet<Vec<String>> = HashSet::new();
        let mut rows = 0_u32;
        let mut missing_total = 0_u32;
        for record in reader.records() {
            let record = record.map_err(csv_error)?;
            rows += 1;
            missing_total += record.iter().filter(|field| field.is_empty()).count() as u32;
            seen.insert(record.iter().map(ToOwned::to_owned).collect());
        }

        let distinct = seen.len() as u32;
        let unit = if headers[mass_index].to_ascii_lowercase().contains("lb") {
            Unit::Lb
        } else {
            Unit::Kg
        };

        Ok(Submission {
            row_count: i64::from(distinct),
            missing_total: i64::from(missing_total),
            duplicate_rows: i64::from(rows - distinct),
            unit,
        })
    }
}

fn csv_error(error: csv::Error) -> SilynError {
    SilynError::Permanent(format!("csv parse failed: {error}"))
}
