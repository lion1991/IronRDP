//! Offline EGFX codec replay for debugging decode failures without a real server.
//!
//! Feed it a directory produced by `NEXSHELL_RDP_EGFX_DUMP=<dir>` (see
//! `src/dump.rs`). It replays every `*.bin` payload, in capture order, back
//! through the *current* ClearCodec / Progressive decoders and prints the
//! success/failure (with full error chain) of each.
//!
//! Usage:
//!   cargo run -p ironrdp-egfx --example egfx_replay -- <dump_dir>

#![allow(clippy::print_stdout, reason = "diagnostic CLI tool: stdout is the output")]

use std::fs;
use std::path::Path;

use ironrdp_graphics::clearcodec::ClearCodecDecoder;
use ironrdp_graphics::progressive::ProgressiveDecoder;

// This example only needs ironrdp-graphics; silence unused-crate-dependencies
// for the rest of the package's deps that the example binary links implicitly.
use bit_field as _;
use bitflags as _;
use ironrdp_core as _;
use ironrdp_dvc as _;
use ironrdp_egfx as _;
use ironrdp_pdu as _;
use tracing as _;

struct Meta {
    codec: String,
    width: u16,
    height: u16,
    context_id: u32,
}

/// Parse `0007_progressive_s1_w2560_h1466_ctx0.bin` style names.
fn parse_name(name: &str) -> Option<Meta> {
    let stem = name.strip_suffix(".bin")?;
    let parts: Vec<&str> = stem.split('_').collect();
    let codec = parts.get(1)?.to_string();
    let mut width = 0;
    let mut height = 0;
    let mut context_id = 0;
    for p in &parts {
        if let Some(v) = p.strip_prefix("ctx") {
            context_id = v.parse().unwrap_or(0);
        } else if let Some(v) = p.strip_prefix('w') {
            if let Ok(v) = v.parse() {
                width = v;
            }
        } else if let Some(v) = p.strip_prefix('h') {
            if let Ok(v) = v.parse() {
                height = v;
            }
        }
    }
    Some(Meta {
        codec,
        width,
        height,
        context_id,
    })
}

fn chain(e: &dyn core::error::Error) -> String {
    let mut out = format!("{e}");
    let mut src = e.source();
    while let Some(inner) = src {
        out.push_str(&format!(" -> {inner}"));
        src = inner.source();
    }
    out
}

fn main() {
    let dir = std::env::args().nth(1).expect("usage: egfx_replay <dump_dir>");

    let mut names: Vec<String> = fs::read_dir(&dir)
        .expect("read dump dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".bin"))
        .collect();
    names.sort(); // seq prefix => capture order (preserves decoder cache continuity)

    let mut clear = ClearCodecDecoder::new();
    let mut prog = ProgressiveDecoder::new();
    let (mut ok, mut fail) = (0u32, 0u32);

    for name in &names {
        let Some(meta) = parse_name(name) else {
            continue;
        };
        let data = match fs::read(Path::new(&dir).join(name)) {
            Ok(d) => d,
            Err(e) => {
                println!("SKIP {name}: read error {e}");
                continue;
            }
        };

        let result: Result<String, String> = match meta.codec.as_str() {
            "clearcodec" => clear
                .decode(&data, meta.width, meta.height)
                .map(|px| format!("{} BGRA bytes", px.len()))
                .map_err(|e| chain(&e)),
            "progressive" => prog
                .decode_bitmap(meta.context_id, meta.width, meta.height, &data)
                .map(|tiles| format!("{} tiles", tiles.len()))
                .map_err(|e| format!("{e}")),
            other => {
                println!("SKIP {name}: unknown codec {other}");
                continue;
            }
        };

        match result {
            Ok(info) => {
                ok += 1;
                println!("OK   {name} ({}x{}) -> {info}", meta.width, meta.height);
            }
            Err(err) => {
                fail += 1;
                println!("FAIL {name} ({}x{}): {err}", meta.width, meta.height);
            }
        }
    }

    println!("\n{} payloads: {ok} ok, {fail} fail", names.len());
}
