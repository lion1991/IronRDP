//! Single-tile numeric trace for the RemoteFX Progressive decoder.
//!
//! Replays a dump dir (as egfx_replay) and, for a target tile, prints per-PDU:
//! pass/quality/prog-BitPos, DWT-coefficient stats (LL3 DC), post-IDWT spatial
//! Y/Cb/Cr range, and the reconstructed RGB histogram (unique-color count).
//!
//! Env: TS=surface_id TX=xIdx TY=yIdx  (defaults 0/14/4).
//!      DUMP_FIRST=1 lists every tile (type,x,y,quality) in the first N PDUs.
//!
//! Usage: cargo run -p ironrdp-egfx --example prog_trace -- <dump_dir>

#![allow(
    clippy::print_stdout,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::as_conversions,
    clippy::similar_names
)]

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use ironrdp_graphics::progressive::ProgressiveDecoder;
use ironrdp_pdu::codecs::rfx::progressive::{ProgressiveBlock, ProgressiveTile, decode_progressive_stream};

use bit_field as _;
use bitflags as _;
use ironrdp_core as _;
use ironrdp_dvc as _;
use ironrdp_egfx as _;
use tracing as _;

fn parse_name(name: &str) -> Option<(String, u16, u16, u16)> {
    let stem = name.strip_suffix(".bin")?;
    let parts: Vec<&str> = stem.split('_').collect();
    let codec = parts.get(1)?.to_string();
    let (mut w, mut h, mut s) = (0u16, 0u16, 0u16);
    for p in &parts {
        if let Some(v) = p.strip_prefix('s') {
            if let Ok(v) = v.parse() {
                s = v;
            }
        } else if let Some(v) = p.strip_prefix('w') {
            if let Ok(v) = v.parse() {
                w = v;
            }
        } else if let Some(v) = p.strip_prefix('h') {
            if let Ok(v) = v.parse() {
                h = v;
            }
        }
    }
    Some((codec, w, h, s))
}

fn stats(vals: impl Iterator<Item = i32>) -> (i32, i32, f64) {
    let (mut mn, mut mx, mut sum, mut n) = (i32::MAX, i32::MIN, 0i64, 0i64);
    for v in vals {
        mn = mn.min(v);
        mx = mx.max(v);
        sum += i64::from(v);
        n += 1;
    }
    if n == 0 {
        return (0, 0, 0.0);
    }
    (mn, mx, sum as f64 / n as f64)
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: prog_trace <dump_dir>");
    let ts: u16 = std::env::var("TS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let tx: u16 = std::env::var("TX").ok().and_then(|v| v.parse().ok()).unwrap_or(14);
    let ty: u16 = std::env::var("TY").ok().and_then(|v| v.parse().ok()).unwrap_or(4);
    let dump_first = std::env::var("DUMP_FIRST").is_ok();

    let mut names: Vec<String> = fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".bin"))
        .collect();
    names.sort();

    let mut prog = ProgressiveDecoder::new();
    println!("target surface={ts} tile=({tx},{ty})");

    for (pdu_idx, name) in names.iter().enumerate() {
        let Some((codec, w, h, s)) = parse_name(name) else {
            continue;
        };
        if codec != "progressive" {
            continue;
        }
        let data = fs::read(Path::new(&dir).join(name)).expect("read");

        if dump_first && pdu_idx < 6 {
            if let Ok(blocks) = decode_progressive_stream(&data) {
                for b in &blocks {
                    if let ProgressiveBlock::Region(r) = b {
                        for t in &r.tiles {
                            let (kind, x, y, q) = match t {
                                ProgressiveTile::Simple(t) => ("SIMPLE", t.x_idx, t.y_idx, 0xFFu16),
                                ProgressiveTile::First(t) => ("FIRST", t.x_idx, t.y_idx, u16::from(t.quality)),
                                ProgressiveTile::Upgrade(t) => ("UPGRADE", t.x_idx, t.y_idx, u16::from(t.quality)),
                            };
                            println!("  pdu{pdu_idx} {kind} ({x},{y}) quality={q}");
                        }
                    }
                }
            }
        }

        // Note the tile type touching the target this PDU, for context.
        let mut touched: Option<&'static str> = None;
        if let Ok(blocks) = decode_progressive_stream(&data) {
            for b in &blocks {
                if let ProgressiveBlock::Region(r) = b {
                    for t in &r.tiles {
                        let (kind, x, y) = match t {
                            ProgressiveTile::Simple(t) => ("SIMPLE", t.x_idx, t.y_idx),
                            ProgressiveTile::First(t) => ("FIRST", t.x_idx, t.y_idx),
                            ProgressiveTile::Upgrade(t) => ("UPGRADE", t.x_idx, t.y_idx),
                        };
                        if s == ts && x == tx && y == ty {
                            touched = Some(kind);
                        }
                    }
                }
            }
        }

        if prog.decode_bitmap(s, w, h, &data).is_err() {
            continue;
        }
        let Some(kind) = touched else { continue };
        let Some(tile) = prog.surface_tile(ts, tx, ty) else {
            continue;
        };

        // Coefficient-domain LL3 (band 9) DC for Y.
        let ll3_off = if tile.use_reduce_extrapolate { 4015 } else { 4032 };
        let (lly_mn, lly_mx, lly_mean) = stats(tile.coefficients[0][ll3_off..4096].iter().map(|&c| i32::from(c)));

        // Post-IDWT spatial ranges.
        let spatial = tile.debug_spatial();
        let (ymn, ymx, ymean) = stats(spatial[0].iter().map(|&c| i32::from(c)));
        let (cbmn, cbmx, _) = stats(spatial[1].iter().map(|&c| i32::from(c)));
        let (crmn, crmx, _) = stats(spatial[2].iter().map(|&c| i32::from(c)));

        // Reconstructed RGB histogram.
        let mut px = vec![0u8; 64 * 64 * 4];
        tile.reconstruct_to_rgba(&mut px);
        let mut colors: HashSet<u32> = HashSet::new();
        let mut rset: HashSet<u8> = HashSet::new();
        for p in px.chunks_exact(4) {
            colors.insert(u32::from(p[0]) | (u32::from(p[1]) << 8) | (u32::from(p[2]) << 16));
            rset.insert(p[0]);
        }

        // prog BitPos Y per band (band order HL1..HH3,LL3).
        let bp: Vec<u8> = (0..10).map(|b| tile.prog_quant[0].for_band(b)).collect();

        println!(
            "pdu{pdu_idx:04} {kind:7} pass={} q={:3} rex={} | LL3y[{lly_mn},{lly_mx}]~{lly_mean:.0} \
             | spatialY[{ymn},{ymx}]~{ymean:.0} Cb[{cbmn},{cbmx}] Cr[{crmn},{crmx}] \
             | uniqRGB={} uniqR={} | progBP_Y={bp:?}",
            tile.pass,
            tile.quality,
            u8::from(tile.use_reduce_extrapolate),
            colors.len(),
            rset.len(),
        );
    }
}
