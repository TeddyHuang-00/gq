//! Determine the availability according to the same set of rules of selection
//! as `gq run`.

use std::io::IsTerminal;

use anyhow::Result;
use serde::Serialize;
use tabled::{
    Table, Tabled,
    builder::Builder,
    settings::{Alignment, Color, Style},
};

use super::Exit;
use crate::{
    data::AllUsage,
    options::{cli::List, output::OutputMode},
    writer,
};

#[derive(Serialize, Tabled)]
struct Row {
    index: usize,
    memory_total_mib: f64,
    memory_used_mib: f64,
    memory_free_mib: f64,
    utilization_percent: f64,
    status: &'static str,
}

/// Formats MiB as gigabytes (`GB = MiB / 1024`) with one decimal.
fn gb(mib: f64) -> String {
    format!("{:.1}", mib / 1024.0)
}

/// Width in display columns of the merged used/free VRAM bar.
const VRAM_BAR_WIDTH: usize = 20;
/// Width in display columns of the utilization bar.
const UTIL_BAR_WIDTH: usize = 10;
/// Pretty table headers, in column order.
const HEADERS: [&str; 5] = [
    "Index",
    "Total VRAM (GB)",
    "Used/Free VRAM (GB)",
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
    let mut used_segment = centered_segment(&gb(used), used_width, if color { ' ' } else { '█' });
    let mut free_segment = centered_segment(&gb(free), free_width, if color { ' ' } else { '░' });
    if color {
        used_segment = Color::BG_BRIGHT_RED.colorize(Color::FG_BLACK.colorize(&used_segment));
        free_segment = Color::BG_BRIGHT_GREEN.colorize(Color::FG_BLACK.colorize(&free_segment));
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

fn status_cell(status: &str, color: bool) -> String {
    if color && status == "Available" {
        Color::FG_BRIGHT_YELLOW.colorize(status)
    } else {
        status.to_owned()
    }
}

fn render_pretty(rows: &[Row], color: bool) -> String {
    let mut table_rows = vec![HEADERS.map(str::to_owned).to_vec()];
    table_rows.extend(rows.iter().map(|row| {
        vec![
            row.index.to_string(),
            gb(row.memory_total_mib),
            vram_bar(row.memory_used_mib, row.memory_free_mib, color),
            utilization_cell(row.utilization_percent),
            status_cell(row.status, color),
        ]
    }));
    let mut table = Table::from(Builder::from(table_rows));
    table.with(Style::modern());
    table.with(Alignment::center());
    table.to_string()
}

impl List {
    pub fn execute(&self) -> Result<Exit> {
        let usage = AllUsage::query(self.filter.probe_count)?;
        let present = usage.iter().map(|(index, _)| index).collect::<Vec<_>>();
        if present.is_empty() {
            anyhow::bail!("nvidia-smi reported no cards");
        }
        self.filter.warn_absent(&present);
        let candidates = usage
            .iter()
            .filter(|(index, _)| self.filter.accepts_index(*index))
            .collect::<Vec<_>>();
        let indices = candidates
            .iter()
            .map(|(index, _)| *index)
            .collect::<Vec<_>>();
        self.filter.log_candidates(&present, &indices);
        self.filter.log_idle_rule();
        if candidates.is_empty() {
            anyhow::bail!(
                "no cards matched the filter; present: {present:?}; check --include/--exclude"
            );
        }
        let rows = candidates
            .iter()
            .map(|&(index, stats)| {
                self.filter.log_idle(index, stats);
                Row {
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
                }
            })
            .collect::<Vec<_>>();
        let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        match self.format {
            OutputMode::Pretty => println!("{}", render_pretty(&rows, color)),
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
                "Total VRAM (GB)",
                "Used/Free VRAM (GB)",
                "Utilization",
                "Status"
            ]
        );
    }

    #[test]
    fn gb_formats_mib_with_one_decimal() {
        assert_eq!(gb(32_607.0), "31.8");
        assert_eq!(gb(6_810.0), "6.7");
        assert_eq!(gb(1024.0), "1.0");
        assert_eq!(gb(0.0), "0.0");
    }

    #[test]
    fn header_row_is_centered() {
        let rows = [Row {
            index: 0,
            memory_total_mib: 32_607.0,
            memory_used_mib: 6_809.0,
            memory_free_mib: 25_391.0,
            utilization_percent: 15.0,
            status: "Available",
        }];
        let table = render_pretty(&rows, false);
        let header = table
            .lines()
            .find(|line| line.contains("Index"))
            .unwrap_or_else(|| panic!("no header line in {table:?}"));
        let cells = header.split('│').collect::<Vec<_>>();
        assert_eq!(cells[1].trim(), "Index");
        assert_eq!(cells[2].trim(), "Total VRAM (GB)");
        assert_eq!(cells[3].trim(), "Used/Free VRAM (GB)");
        assert_eq!(cells[4].trim(), "Utilization");
        assert_eq!(cells[5].trim(), "Status");
        // "Utilization" is 11 columns in a 15-column cell: two spaces each
        // side plus the one-column padding. Left alignment would put one
        // space on the left and five on the right.
        assert_eq!(cells[4], "   Utilization   ");
    }

    #[test]
    fn every_cell_is_centered_and_only_available_is_yellow() {
        let rows = [
            Row {
                index: 0,
                memory_total_mib: 32_607.0,
                memory_used_mib: 6_809.0,
                memory_free_mib: 25_391.0,
                utilization_percent: 15.0,
                status: "Available",
            },
            Row {
                index: 1,
                memory_total_mib: 32_607.0,
                memory_used_mib: 30_000.0,
                memory_free_mib: 2_607.0,
                utilization_percent: 100.0,
                status: "In use",
            },
        ];

        let plain = render_pretty(&rows, false);
        assert!(!plain.contains('\u{1b}'), "plain={plain:?}");
        let available = plain
            .lines()
            .find(|line| line.contains("Available"))
            .unwrap_or_else(|| panic!("no Available line in {plain:?}"));
        let cells = available.split('│').collect::<Vec<_>>();
        // The index column is five columns wide for the "Index" header, so a
        // one-character value gets two spaces on each side plus cell padding.
        assert_eq!(cells[1], "   0   ");
        assert_eq!(cells[5], " Available ");

        let colored = render_pretty(&rows, true);
        let lines = colored.lines().collect::<Vec<_>>();
        let available = lines
            .iter()
            .find(|line| line.contains("Available"))
            .unwrap_or_else(|| panic!("no Available line in {colored:?}"));
        let cells = available.split('│').collect::<Vec<_>>();
        assert_eq!(cells[1], "   0   ");
        assert_eq!(cells[5], " \u{1b}[93mAvailable\u{1b}[39m ");

        let in_use = lines
            .iter()
            .find(|line| line.contains("In use"))
            .unwrap_or_else(|| panic!("no In use line in {colored:?}"));
        let cells = in_use.split('│').collect::<Vec<_>>();
        assert_eq!(cells[5].trim(), "In use");
        assert!(!cells[5].contains('\u{1b}'), "in_use={:?}", cells[5]);
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
        let cell = strip_ansi(&vram_bar(1024.0, 1024.0, false));
        let chars = cell.chars().collect::<Vec<_>>();
        // "1.0" centered in each 10-wide segment: 3 fill, 3 digits, 4 fill.
        let (used, free) = chars.split_at(10);
        assert_eq!(used.iter().collect::<String>(), "███1.0████");
        assert_eq!(free.iter().collect::<String>(), "░░░1.0░░░░");
        assert_eq!(run_len(&chars, '█'), 7);
        assert_eq!(run_len(&chars, '░'), 7);

        // 30 of 100 GiB: six used columns, fourteen free.
        let chars = strip_ansi(&vram_bar(3072.0, 7168.0, false))
            .chars()
            .collect::<Vec<_>>();
        assert_eq!(chars[..6].iter().collect::<String>(), "█3.0██");
        assert_eq!(chars[6..].iter().collect::<String>(), "░░░░░7.0░░░░░░");

        let colored = strip_ansi(&vram_bar(512.0, 1536.0, true));
        let chars = colored.chars().collect::<Vec<_>>();
        assert_eq!(chars.len(), VRAM_BAR_WIDTH);
        assert_eq!(chars.iter().filter(|c| c.is_ascii_digit()).count(), 4);
        assert!(colored.starts_with(" 0.5 "), "colored={colored:?}");
    }

    #[test]
    fn colored_bar_uses_black_on_bright_backgrounds() {
        let cell = vram_bar(1024.0, 3072.0, true);
        for code in ["\u{1b}[101m", "\u{1b}[102m", "\u{1b}[30m"] {
            assert!(cell.contains(code), "missing {code:?} in {cell:?}");
        }
        for code in ["\u{1b}[37m", "\u{1b}[97m"] {
            assert!(!cell.contains(code), "unexpected {code:?} in {cell:?}");
        }
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
