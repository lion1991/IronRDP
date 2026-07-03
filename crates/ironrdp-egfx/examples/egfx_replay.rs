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

use std::collections::HashMap;
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
    surface_id: u16,
}

/// Parse `0007_progressive_s1_w2560_h1466_ctx0.bin` style names.
fn parse_name(name: &str) -> Option<Meta> {
    let stem = name.strip_suffix(".bin")?;
    let parts: Vec<&str> = stem.split('_').collect();
    let codec = parts.get(1)?.to_string();
    let mut width = 0;
    let mut height = 0;
    let mut surface_id = 0;
    for p in &parts {
        if let Some(v) = p.strip_prefix('s') {
            // `s<N>` surface id (skip `ctx...` / stray tokens that also start with 's').
            if let Ok(v) = v.parse() {
                surface_id = v;
            }
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
        surface_id,
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

/// Per-surface RGB canvas for compositing decoded progressive tiles.
struct Canvas {
    w: usize,
    h: usize,
    rgb: Vec<u8>,
}

/// Blit a 64x64 RGBA tile into the canvas at (x_idx*64, y_idx*64), clipping to bounds.
fn blit_tile(canvas: &mut Canvas, x_idx: u16, y_idx: u16, pixels: &[u8]) {
    let ox = usize::from(x_idx) * 64;
    let oy = usize::from(y_idx) * 64;
    for ty in 0..64 {
        let py = oy + ty;
        if py >= canvas.h {
            break;
        }
        for tx in 0..64 {
            let px = ox + tx;
            if px >= canvas.w {
                break;
            }
            let src = (ty * 64 + tx) * 4;
            let dst = (py * canvas.w + px) * 3;
            canvas.rgb[dst] = pixels[src];
            canvas.rgb[dst + 1] = pixels[src + 1];
            canvas.rgb[dst + 2] = pixels[src + 2];
        }
    }
}

/// Write a canvas as a binary PPM (P6).
fn write_ppm(path: &Path, canvas: &Canvas) {
    let mut out = format!("P6\n{} {}\n255\n", canvas.w, canvas.h).into_bytes();
    out.extend_from_slice(&canvas.rgb);
    if let Err(e) = fs::write(path, &out) {
        println!("PPM write error {}: {e}", path.display());
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut dir = None;
    let mut ppm_dir: Option<String> = None;
    while let Some(a) = args.next() {
        if a == "--ppm-dir" {
            ppm_dir = args.next();
        } else {
            dir = Some(a);
        }
    }
    let dir = dir.expect("usage: egfx_replay <dump_dir> [--ppm-dir <out>]");
    if let Some(pd) = &ppm_dir {
        fs::create_dir_all(pd).expect("create ppm dir");
    }
    let mut canvases: HashMap<u16, Canvas> = HashMap::new();

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
                .decode_bitmap(meta.surface_id, meta.width, meta.height, &data)
                .map(|tiles| {
                    if let Some(pd) = &ppm_dir {
                        let canvas = canvases.entry(meta.surface_id).or_insert_with(|| Canvas {
                            w: usize::from(meta.width),
                            h: usize::from(meta.height),
                            rgb: vec![0u8; usize::from(meta.width) * usize::from(meta.height) * 3],
                        });
                        for t in &tiles {
                            blit_tile(canvas, t.x_idx, t.y_idx, &t.pixels);
                        }
                        let stem = name.strip_suffix(".bin").unwrap_or(name);
                        write_ppm(&Path::new(pd).join(format!("{stem}.ppm")), canvas);
                    }
                    format!("{} tiles", tiles.len())
                })
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
