//! Sequential ClearCodec replay hunting silent all-black decodes.
//!
//! Replays every `*_clearcodec_*.bin` of a dump dir (capture order) through one
//! stateful `ClearCodecDecoder`; for each payload prints header layer sizes
//! (glyph flags, residual/bands/subcodec byte counts) and flags outputs that
//! are entirely black despite a non-trivial area.
//!
//! Usage: cargo run -p ironrdp-egfx --example clear_black -- <dump_dir>

#![allow(clippy::print_stdout, clippy::as_conversions, clippy::cast_possible_truncation)]

use std::fs;

use ironrdp_graphics::clearcodec::ClearCodecDecoder;

use bit_field as _;
use bitflags as _;
use ironrdp_core as _;
use ironrdp_dvc as _;
use ironrdp_egfx as _;
use ironrdp_graphics as _;
use ironrdp_pdu as _;
use tracing as _;

fn parse_wh(name: &str) -> Option<(u16, u16)> {
    let mut w = None;
    let mut h = None;
    for p in name.trim_end_matches(".bin").split('_') {
        if let Some(v) = p.strip_prefix('w') {
            w = v.parse().ok().or(w);
        } else if let Some(v) = p.strip_prefix('h') {
            h = v.parse().ok().or(h);
        }
    }
    Some((w?, h?))
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: clear_black <dump_dir>");
    let mut files: Vec<_> = fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("clearcodec") && n.ends_with(".bin"))
        })
        .collect();
    files.sort();

    let mut dec = ClearCodecDecoder::new();
    let mut black = 0usize;
    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let Some((w, h)) = parse_wh(&name) else { continue };
        let data = fs::read(path).expect("read");
        // Header: flags(1) seq(1) residual(4) bands(4) subcodec(4)
        let (flags, res, bands, sub) = if data.len() >= 14 {
            (
                data[0],
                u32::from_le_bytes([data[2], data[3], data[4], data[5]]),
                u32::from_le_bytes([data[6], data[7], data[8], data[9]]),
                u32::from_le_bytes([data[10], data[11], data[12], data[13]]),
            )
        } else {
            (data.first().copied().unwrap_or(0), 0, 0, 0)
        };
        // 黑 payload 的 subcodec 类型分布
        let layer_end = 14usize
            .checked_add(res as usize)
            .and_then(|v| v.checked_add(bands as usize))
            .and_then(|v| v.checked_add(sub as usize));
        let sub_kinds: Vec<String> = if sub > 0 && layer_end.is_some_and(|e| data.len() >= e) {
            let start = 14 + res as usize + bands as usize;
            ironrdp_pdu::codecs::clearcodec::decode_subcodec_layer(&data[start..start + sub as usize])
                .map(|subs| subs.iter().map(|s| format!("{:?}({}x{})", s.codec_id, s.width, s.height)).collect())
                .unwrap_or_else(|e| vec![format!("parse-err {e}")])
        } else {
            Vec::new()
        };
        match dec.decode(&data, w, h) {
            Ok(px) => {
                let is_black = px.chunks_exact(4).all(|p| p[0] < 8 && p[1] < 8 && p[2] < 8);
                if is_black && usize::from(w) * usize::from(h) >= 1024 {
                    black += 1;
                    println!(
                        "{name}: ALL-BLACK flags={flags:#04x} res={res} bands={bands} sub={sub} bytes={} kinds={}",
                        data.len(), sub_kinds.join(",")
                    );
                }
            }
            Err(e) => println!("{name}: ERR {e}"),
        }
    }
    println!("done, {black} suspicious all-black payloads");
}
