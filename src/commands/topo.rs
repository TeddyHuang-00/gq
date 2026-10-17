//! Find out the topology of the GPUs.

use std::{collections::BTreeMap, io::IsTerminal};

use clap::Args;
use tabled::{
    Table,
    builder::Builder,
    settings::{Color, Style, object::Cell},
};

use super::Exit;
use crate::{
    options::output::OutputMode,
    query::{LinkType, Quality, Topology as QueryTopology},
    writer,
};

/// Queries all GPUs and build the topology.
#[derive(Args, Debug)]
pub struct Topology {
    /// Output format
    #[arg(short, long, default_value = "pretty")]
    pub format: OutputMode,
}

impl Topology {
    pub fn execute(&self) -> anyhow::Result<Exit> {
        let topology = QueryTopology::query()?;
        let n = topology.dimensions();
        if n == 0 {
            anyhow::bail!("nvidia-smi reported no GPUs");
        }
        if n == 1 {
            log::warn!("only one GPU detected; topology cannot be determined");
            return Ok(Exit::Ok);
        }
        let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        match self.format {
            OutputMode::Pretty => print_pretty(&topology, color),
            OutputMode::Json => {
                let map = link_map(n, |a, b| topology.connection(a, b));
                println!("{}", writer::json::render(&map)?);
            }
            OutputMode::Csv => {
                let rows = link_rows(n, |a, b| topology.connection(a, b));
                print!(
                    "{}",
                    writer::csv::render(
                        std::iter::once(CSV_HEADER).chain(rows.iter().map(LinkRow::fields))
                    )
                );
            }
        }
        Ok(Exit::Ok)
    }
}

const CSV_HEADER: [&str; 4] = ["card a", "card b", "link", "quality"];

struct LinkRow {
    card_a: String,
    card_b: String,
    link: String,
    quality: String,
}

impl LinkRow {
    const fn fields(&self) -> [&str; 4] {
        [
            self.card_a.as_str(),
            self.card_b.as_str(),
            self.link.as_str(),
            self.quality.as_str(),
        ]
    }
}

fn link_rows(
    dimensions: usize,
    mut connection: impl FnMut(usize, usize) -> LinkType,
) -> Vec<LinkRow> {
    let mut rows = Vec::new();
    for card_a in 0..dimensions {
        for card_b in card_a + 1..dimensions {
            let link = connection(card_a, card_b);
            rows.push(LinkRow {
                card_a: format!("GPU{card_a}"),
                card_b: format!("GPU{card_b}"),
                link: link_label(link),
                quality: link
                    .quality()
                    .map_or_else(String::new, |quality| quality.to_string()),
            });
        }
    }
    rows
}

#[derive(serde::Serialize)]
struct LinkInfo {
    link: String,
    quality: String,
}

fn link_map(
    dimensions: usize,
    mut connection: impl FnMut(usize, usize) -> LinkType,
) -> BTreeMap<String, BTreeMap<String, LinkInfo>> {
    let mut cards = BTreeMap::new();
    for card in 0..dimensions {
        let mut peers = BTreeMap::new();
        for peer in 0..dimensions {
            if peer == card {
                continue;
            }
            let link = connection(card, peer);
            peers.insert(
                format!("GPU{peer}"),
                LinkInfo {
                    link: link_label(link),
                    quality: link
                        .quality()
                        .map_or_else(String::new, |quality| quality.to_string()),
                },
            );
        }
        cards.insert(format!("GPU{card}"), peers);
    }
    cards
}

fn print_pretty(topology: &QueryTopology, color: bool) {
    let dimensions = topology.dimensions();
    let mut rows = Vec::with_capacity(dimensions + 1);
    let mut header = vec![String::new()];
    header.extend((0..dimensions).map(|index| index.to_string()));
    rows.push(header);
    for row in 0..dimensions {
        let mut cells = vec![row.to_string()];
        cells.extend((0..dimensions).map(|column| {
            let link = topology.connection(row, column);
            match link {
                LinkType::X => String::new(),
                _ if color => "  ".to_owned(),
                _ => link
                    .quality()
                    .map_or_else(String::new, |quality| quality_letter(quality).to_owned()),
            }
        }));
        rows.push(cells);
    }

    let mut table = Table::from(Builder::from(rows));
    table.with(Style::modern());
    if color {
        for row in 0..dimensions {
            for column in 0..dimensions {
                if let Some(cell_color) = link_color(topology.connection(row, column)) {
                    table.modify(Cell::new(row + 1, column + 1), cell_color);
                }
            }
        }
    }
    println!("{table}");
    print_legend(color);
}

const fn link_color(link: LinkType) -> Option<Color> {
    match link {
        LinkType::X => None,
        LinkType::System => Some(Color::BG_RED),
        LinkType::NVLink(_) => Some(Color::BG_GREEN),
        _ => Some(Color::BG_YELLOW),
    }
}

const fn quality_letter(quality: Quality) -> &'static str {
    match quality {
        Quality::High => "H",
        Quality::Medium => "M",
        Quality::Low => "L",
    }
}

fn print_legend(color: bool) {
    if color {
        println!(
            "Color represents connection quality:\n{}: NV# (# NVLinks)\n{}: NODE (same node), PHB (host bridge), PXB (multiple bridges), or PIX (single bridge)\n{}: SYS (system-level)",
            Color::BG_GREEN.colorize("  "),
            Color::BG_YELLOW.colorize("  "),
            Color::BG_RED.colorize("  "),
        );
    } else {
        println!("H: NV# (high)\nM: NODE, PHB, PXB, or PIX (medium)\nL: SYS (low)");
    }
}

fn link_label(link: LinkType) -> String {
    match link {
        LinkType::X => String::new(),
        LinkType::System => "SYS".to_owned(),
        LinkType::SameNode => "NODE".to_owned(),
        LinkType::HostBridge => "PHB".to_owned(),
        LinkType::MultipleBridges => "PXB".to_owned(),
        LinkType::SingleBridge => "PIX".to_owned(),
        LinkType::NVLink(count) => format!("NV{count}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_letters_are_h_m_l() {
        assert_eq!(quality_letter(Quality::High), "H");
        assert_eq!(quality_letter(Quality::Medium), "M");
        assert_eq!(quality_letter(Quality::Low), "L");
    }

    const LINKS: [[LinkType; 3]; 3] = [
        [LinkType::X, LinkType::NVLink(4), LinkType::System],
        [LinkType::NVLink(4), LinkType::X, LinkType::SameNode],
        [LinkType::System, LinkType::SameNode, LinkType::X],
    ];

    #[test]
    fn csv_rows_emit_each_card_pair_once_with_link_grading() {
        let rows = link_rows(3, |a, b| LINKS[a][b]);

        assert_eq!(
            writer::csv::render(
                std::iter::once(CSV_HEADER).chain(rows.iter().map(LinkRow::fields))
            ),
            "card a,card b,link,quality\nGPU0,GPU1,NV4,High\nGPU0,GPU2,SYS,Low\nGPU1,GPU2,NODE,Medium\n"
        );
    }

    #[test]
    fn json_map_nests_link_and_quality_under_every_other_card() {
        let map = link_map(3, |a, b| LINKS[a][b]);
        let Ok(rendered) = writer::json::render(&map) else {
            panic!("link map must serialize");
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&rendered) else {
            panic!("rendered JSON must parse: {rendered}");
        };

        let Some(cards) = json.as_object() else {
            panic!("top level must be an object: {rendered}");
        };
        assert_eq!(
            cards.keys().map(String::as_str).collect::<Vec<_>>(),
            ["GPU0", "GPU1", "GPU2"]
        );
        assert_eq!(json["GPU0"].as_object().map_or(0, serde_json::Map::len), 2);
        assert!(json["GPU0"]["GPU0"].is_null());
        assert_eq!(json["GPU0"]["GPU1"]["link"], "NV4");
        assert_eq!(json["GPU0"]["GPU1"]["quality"], "High");
        assert_eq!(json["GPU0"]["GPU2"]["quality"], "Low");
        assert_eq!(json["GPU1"]["GPU0"]["link"], "NV4");
        assert_eq!(json["GPU1"]["GPU2"]["quality"], "Medium");
    }
}
