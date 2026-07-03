//! Dump-dir progressive coverage report: per PDU prints region rects and tile
//! indices; then reports 64px tiles of a W×H surface never covered by any tile.
//!
//! Usage: cargo run -p ironrdp-egfx --example prog_coverage -- <dump_dir> [W H]

#![allow(clippy::print_stdout, clippy::as_conversions, clippy::cast_possible_truncation)]

use std::collections::HashSet;
use std::fs;

use ironrdp_pdu::codecs::rfx::progressive::{decode_progressive_stream, ProgressiveBlock, ProgressiveTile};

use bit_field as _;
use bitflags as _;
use ironrdp_core as _;
use ironrdp_dvc as _;
use ironrdp_egfx as _;
use ironrdp_graphics as _;
use tracing as _;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("usage: prog_coverage <dump_dir> [W H]");
    let w: u16 = args.next().and_then(|v| v.parse().ok()).unwrap_or(1920);
    let h: u16 = args.next().and_then(|v| v.parse().ok()).unwrap_or(1080);

    let mut files: Vec<_> = fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("progressive") && n.ends_with(".bin"))
        })
        .collect();
    files.sort();

    let mut covered: HashSet<(u16, u16)> = HashSet::new();
    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let data = fs::read(path).expect("read bin");
        let blocks = match decode_progressive_stream(&data) {
            Ok(b) => b,
            Err(e) => {
                println!("{name}: PARSE FAIL {e}");
                continue;
            }
        };
        let mut rects = Vec::new();
        let mut tiles = Vec::new();
        for b in &blocks {
            if let ProgressiveBlock::Region(r) = b {
                for rect in &r.rects {
                    rects.push(format!("({},{})+{}x{}", rect.x, rect.y, rect.width, rect.height));
                }
                for t in &r.tiles {
                    let (x, y, kind) = match t {
                        ProgressiveTile::Simple(t) => (t.x_idx, t.y_idx, "S"),
                        ProgressiveTile::First(t) => (t.x_idx, t.y_idx, "F"),
                        ProgressiveTile::Upgrade(t) => (t.x_idx, t.y_idx, "U"),
                    };
                    covered.insert((x, y));
                    tiles.push(format!("{kind}{x},{y}"));
                }
            }
        }
        println!(
            "{name}: rects[{}]={} tiles[{}]={}",
            rects.len(),
            rects.join(" "),
            tiles.len(),
            if tiles.len() > 40 {
                format!("{}...", tiles[..40].join(" "))
            } else {
                tiles.join(" ")
            }
        );
    }

    let (tw, th) = (w.div_ceil(64), h.div_ceil(64));
    let missing: Vec<_> = (0..th)
        .flat_map(|ty| (0..tw).map(move |tx| (tx, ty)))
        .filter(|k| !covered.contains(k))
        .collect();
    println!(
        "\ncovered {} / {} tiles; missing {}: {:?}",
        covered.len(),
        usize::from(tw) * usize::from(th),
        missing.len(),
        missing
    );
}
