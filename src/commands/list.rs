//! Determine the availability according to the same set of rules of selection
//! as `gq run`.

use anyhow::Result;
use clap::Args;
use serde::Serialize;
use tabled::{Table, Tabled};

use super::Exit;
use crate::{
    options::{output::OutputMode, select::Filter},
    query::AllUsage,
    writer,
};

/// List GPUs and their status. Alternative to `nvidia-smi`
#[derive(Args, Debug)]
pub struct List {
    /// Output format
    #[arg(short, long, default_value = "pretty")]
    pub format: OutputMode,

    #[command(flatten, next_help_heading = "Filter")]
    pub filter: Filter,
}

#[derive(Serialize, Tabled)]
struct Row {
    index: usize,
    #[tabled(display = "pretty_number")]
    memory_total_mib: f64,
    #[tabled(display = "pretty_number")]
    memory_used_mib: f64,
    #[tabled(display = "pretty_number")]
    memory_free_mib: f64,
    #[tabled(display = "pretty_number")]
    utilization_percent: f64,
    status: &'static str,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn pretty_number(value: &f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

impl List {
    pub fn execute(&self) -> Result<Exit> {
        let usage = AllUsage::query(self.filter.probe_count)?;
        let present = usage.iter().map(|(index, _)| index).collect::<Vec<_>>();
        if present.is_empty() {
            anyhow::bail!("nvidia-smi reported no cards");
        }
        self.filter.warn_absent(&present);
        let rows = usage
            .iter()
            .filter(|(index, _)| self.filter.accepts_index(*index))
            .map(|(index, stats)| Row {
                index,
                memory_total_mib: stats.memory_total_mib,
                memory_used_mib: stats.memory_used_mib,
                memory_free_mib: stats.memory_free_mib,
                utilization_percent: stats.utilization_percent,
                status: if self.filter.accepts_usage(stats) {
                    "Available"
                } else {
                    "In use"
                },
            })
            .collect::<Vec<_>>();
        if rows.is_empty() {
            anyhow::bail!("no cards matched the filter");
        }
        match self.format {
            OutputMode::Pretty => println!("{}", Table::new(rows)),
            OutputMode::Json => println!("{}", writer::json::render(&rows)?),
            OutputMode::Csv => print!("{}", writer::csv::render(csv_rows(&rows))),
        }
        Ok(Exit::Ok)
    }
}

fn csv_rows(rows: &[Row]) -> Vec<Vec<String>> {
    let mut output = vec![
        [
            "index",
            "memory_total_mib",
            "memory_used_mib",
            "memory_free_mib",
            "utilization_percent",
            "status",
        ]
        .map(str::to_owned)
        .to_vec(),
    ];
    output.extend(rows.iter().map(|row| {
        vec![
            row.index.to_string(),
            row.memory_total_mib.to_string(),
            row.memory_used_mib.to_string(),
            row.memory_free_mib.to_string(),
            row.utilization_percent.to_string(),
            row.status.to_owned(),
        ]
    }));
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_serializes_as_json_object_and_csv_record() {
        let rows = [Row {
            index: 2,
            memory_total_mib: 100.0,
            memory_used_mib: 25.5,
            memory_free_mib: 74.5,
            utilization_percent: 12.0,
            status: "In use",
        }];
        let json =
            writer::json::render(&rows).unwrap_or_else(|err| panic!("Failed to write json: {err}"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json)
                .unwrap_or_else(|err| panic!("Failed to load json: {err}"))[0]["status"],
            "In use"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json)
                .unwrap_or_else(|err| panic!("Failed to load json: {err}"))[0]["memory_total_mib"],
            100.0
        );
        assert_eq!(
            writer::csv::render(csv_rows(&rows)),
            "index,memory_total_mib,memory_used_mib,memory_free_mib,utilization_percent,status\n2,100,25.5,74.5,12,In use\n"
        );
    }
}
