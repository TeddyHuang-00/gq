//! Determine the availability according to the same set of rules of selection
//! as `gq run`.

use std::io::IsTerminal;

use anyhow::Result;
use clap::Args;
use serde::Serialize;
use tabled::{
    Table, Tabled,
    builder::Builder,
    settings::{Color, Style},
};

use super::Exit;
use crate::{
    options::{filter::Filter, output::OutputMode},
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

/// Width in display columns of the merged used/free VRAM bar.
const VRAM_BAR_WIDTH: usize = 20;
/// Width in display columns of the utilization bar.
const UTIL_BAR_WIDTH: usize = 10;
/// Pretty table headers, in column order.
const HEADERS: [&str; 5] = [
    "Index",
    "Total VRAM",
    "Used/Free VRAM",
    "Utilization",
    "Status",
];

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn bar_width(fraction: f64, width: usize) -> usize {
    (fraction * width as f64).round().clamp(0.0, width as f64) as usize
}

/// Centers `text` in `width` columns, padding with `fill`. Text that does not
/// fit prints as a bare fill run so proportions are never distorted.
fn centered_segment(text: &str, width: usize, fill: char) -> String {
    let len = text.chars().count();
    if len > width {
        return fill.to_string().repeat(width);
    }
    let left = (width - len) / 2;
    let mut segment = fill.to_string().repeat(left);
    segment.push_str(text);
    segment.push_str(&fill.to_string().repeat(width - len - left));
    segment
}

fn vram_bar(used: f64, free: f64, color: bool) -> String {
    let total = used + free;
    let used_width = if total > 0.0 {
        bar_width(used / total, VRAM_BAR_WIDTH)
    } else {
        VRAM_BAR_WIDTH / 2
    };
    let free_width = VRAM_BAR_WIDTH - used_width;
    let mut used_segment = centered_segment(
        &pretty_number(&used),
        used_width,
        if color { ' ' } else { '█' },
    );
    let mut free_segment = centered_segment(
        &pretty_number(&free),
        free_width,
        if color { ' ' } else { '░' },
    );
    if color {
        used_segment = Color::BG_RED.colorize(Color::FG_WHITE.colorize(&used_segment));
        free_segment = Color::BG_BLUE.colorize(Color::FG_WHITE.colorize(&free_segment));
    }
    format!("{used_segment}{free_segment}")
}

fn utilization_cell(percent: f64) -> String {
    let filled = bar_width(percent / 100.0, UTIL_BAR_WIDTH);
    // `.round()` keeps the number at most 4 columns and rounds halves up;
    // `{:.0}` alone rounds 12.5 to 12 (banker's rounding).
    let number = format!("{:.0}%", percent.round());
    format!(
        "{number:>4} {}{}",
        "█".repeat(filled),
        "▒".repeat(UTIL_BAR_WIDTH - filled)
    )
}

fn print_pretty(rows: &[Row], color: bool) {
    let mut table_rows = vec![HEADERS.map(str::to_owned).to_vec()];
    table_rows.extend(rows.iter().map(|row| {
        vec![
            row.index.to_string(),
            pretty_number(&row.memory_total_mib),
            vram_bar(row.memory_used_mib, row.memory_free_mib, color),
            utilization_cell(row.utilization_percent),
            row.status.to_owned(),
        ]
    }));
    let mut table = Table::from(Builder::from(table_rows));
    table.with(Style::modern());
    println!("{table}");
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
        let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        match self.format {
            OutputMode::Pretty => print_pretty(&rows, color),
            OutputMode::Json => println!("{}", writer::json::render(&rows)?),
            OutputMode::Csv => print!("{}", writer::csv::render(Row::headers(), &rows)?),
        }
        Ok(Exit::Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::while_let_on_iterator)]
    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(ch);
            }
        }
        out
    }

    fn run_len(chars: &[char], ch: char) -> usize {
        chars.iter().filter(|c| **c == ch).count()
    }

    #[test]
    fn headers_match_the_documented_columns() {
        assert_eq!(
            HEADERS,
            [
                "Index",
                "Total VRAM",
                "Used/Free VRAM",
                "Utilization",
                "Status"
            ]
        );
    }

    #[test]
    fn vram_bar_is_twenty_columns_in_both_modes() {
        for (used, free) in [(25.0, 75.0), (6_809.0, 25_391.0), (5.0, 32_000.0)] {
            for color in [false, true] {
                let cell = vram_bar(used, free, color);
                assert_eq!(
                    strip_ansi(&cell).chars().count(),
                    VRAM_BAR_WIDTH,
                    "used={used} free={free} color={color}"
                );
            }
        }
    }

    #[test]
    fn vram_segments_are_proportional_and_centered() {
        let cell = strip_ansi(&vram_bar(50.0, 50.0, false));
        let chars = cell.chars().collect::<Vec<_>>();
        // "50" centered in each 10-wide segment: 4 fill, 2 digits, 4 fill.
        let (used, free) = chars.split_at(10);
        assert_eq!(used.iter().collect::<String>(), "████50████");
        assert_eq!(free.iter().collect::<String>(), "░░░░50░░░░");
        assert_eq!(run_len(&chars, '█'), 8);
        assert_eq!(run_len(&chars, '░'), 8);

        // 30 of 100 MiB: six used columns, fourteen free.
        let chars = strip_ansi(&vram_bar(30.0, 70.0, false))
            .chars()
            .collect::<Vec<_>>();
        assert_eq!(chars[..6].iter().collect::<String>(), "██30██");
        assert_eq!(chars[6..].iter().collect::<String>(), "░░░░░░70░░░░░░");

        let colored = strip_ansi(&vram_bar(25.0, 75.0, true));
        let chars = colored.chars().collect::<Vec<_>>();
        assert_eq!(chars.len(), VRAM_BAR_WIDTH);
        assert_eq!(chars.iter().filter(|c| c.is_ascii_digit()).count(), 4);
        assert!(colored.starts_with(" 25  "));
    }

    #[test]
    fn utilization_cell_is_number_then_ten_column_bar() {
        for percent in [0.0, 12.0, 30.0, 53.0, 100.0] {
            let cell = utilization_cell(percent);
            assert_eq!(
                cell.chars().count(),
                4 + 1 + UTIL_BAR_WIDTH,
                "percent={percent}"
            );
            let chars = cell.chars().collect::<Vec<_>>();
            assert_eq!(chars[4], ' ');
        }
        let chars = utilization_cell(30.0).chars().collect::<Vec<_>>();
        assert_eq!(chars[..4].iter().collect::<String>(), " 30%");
        assert_eq!(run_len(&chars[5..], '█'), 3);
        assert_eq!(run_len(&chars[5..], '▒'), 7);
        assert_eq!(
            run_len(&utilization_cell(100.0).chars().collect::<Vec<_>>(), '█'),
            10
        );
        assert_eq!(
            run_len(&utilization_cell(0.0).chars().collect::<Vec<_>>(), '▒'),
            10
        );
        assert_eq!(
            utilization_cell(100.0).chars().take(4).collect::<String>(),
            "100%"
        );
    }

    #[test]
    fn utilization_cell_rounds_fractional_percents_to_four_columns() {
        let cell = utilization_cell(1.5);
        assert_eq!(cell.chars().count(), 4 + 1 + UTIL_BAR_WIDTH);
        assert!(cell.starts_with("  2% "), "cell={cell:?}");

        let cell = utilization_cell(12.5);
        assert_eq!(cell.chars().count(), 4 + 1 + UTIL_BAR_WIDTH);
        assert!(cell.starts_with(" 13% "), "cell={cell:?}");

        for percent in [0.4, 1.5, 12.5, 99.5, 100.0] {
            let chars = utilization_cell(percent).chars().collect::<Vec<_>>();
            assert_eq!(chars.len(), 15, "percent={percent}");
            assert!(
                chars[5..].iter().all(|c| *c == '█' || *c == '▒'),
                "percent={percent} bar={:?}",
                chars[5..].iter().collect::<String>()
            );
        }
    }

    #[test]
    fn degenerate_vram_inputs_keep_the_bar_whole() {
        for (used, free) in [
            (0.0, 0.0),
            (0.0, 100.0),
            (100.0, 0.0),
            (0.0, 32_000.0),
            (f64::MAX, 1.0),
            (1.0, f64::MAX),
        ] {
            for color in [false, true] {
                let cell = vram_bar(used, free, color);
                assert_eq!(
                    strip_ansi(&cell).chars().count(),
                    VRAM_BAR_WIDTH,
                    "used={used} free={free} color={color}"
                );
            }
        }
        // A value wider than its segment drops the number and prints the bar
        // only.
        assert_eq!(
            strip_ansi(&vram_bar(f64::MAX, 1.0, false)),
            "█".repeat(VRAM_BAR_WIDTH)
        );
    }

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
            writer::csv::render(Row::headers(), &rows)
                .unwrap_or_else(|err| panic!("Failed to write csv: {err}")),
            "index,memory_total_mib,memory_used_mib,memory_free_mib,utilization_percent,status\n2,100.0,25.5,74.5,12.0,In use\n"
        );
    }
}
