//! Env-gated raw EGFX codec payload dumper for offline replay debugging.
//!
//! Enable with `NEXSHELL_RDP_EGFX_DUMP=<dir>`. When set, the exact decoder-input
//! byte slices for the first ~40 ClearCodec (`WireToSurface1`) and first ~2000
//! Progressive (`WireToSurface2`) payloads are written as `<seq>_<codec>_...bin`
//! plus one `meta.jsonl` line each (codec, surface, dims, context id, caps,
//! runtime error). Replay them offline with the `egfx_replay` example.
//!
//! All IO errors are swallowed: dumping must never disturb a live session.

use core::sync::atomic::{AtomicU64, Ordering};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Per-codec cap so a long session doesn't fill the disk; the first frames
/// (including the cache-independent first PDU) are the diagnostic ones.
const MAX_CLEAR: u64 = 40;
/// Progressive needs a much wider window: the video-playback segment (heavy
/// diff tiles) starts well past the page-load prefix. Override via
/// `IRONRDP_EGFX_DUMP_PROG_CAP`.
const MAX_PROG: u64 = 2000;

fn prog_cap() -> u64 {
    static CAP: OnceLock<u64> = OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("IRONRDP_EGFX_DUMP_PROG_CAP")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(MAX_PROG)
    })
}

fn clear_cap() -> u64 {
    static CAP: OnceLock<u64> = OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("IRONRDP_EGFX_DUMP_CLEAR_CAP")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(MAX_CLEAR)
    })
}

fn dir() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let raw = std::env::var_os("NEXSHELL_RDP_EGFX_DUMP")?;
        let path = PathBuf::from(raw);
        fs::create_dir_all(&path).ok()?;
        Some(path)
    })
    .as_ref()
}

/// Cheap check so callers can skip building metadata (caps string, error chain)
/// on the hot path when dumping is disabled.
pub(crate) fn enabled() -> bool {
    dir().is_some()
}

static SEQ: AtomicU64 = AtomicU64::new(0);
static CLEAR_N: AtomicU64 = AtomicU64::new(0);
static PROG_N: AtomicU64 = AtomicU64::new(0);

/// One dumped codec payload plus its metadata.
pub(crate) struct DumpRecord<'a> {
    /// `"clearcodec"` or `"progressive"`.
    pub codec: &'a str,
    pub surface_id: u16,
    /// Decoder-input width (ClearCodec: dest rect width; Progressive: surface width).
    pub width: u16,
    pub height: u16,
    /// Progressive codec context id; `None` for ClearCodec.
    pub context_id: Option<u32>,
    pub caps: &'a str,
    pub data: &'a [u8],
    /// Runtime decode error (full chain) if the live decode failed; `None` if it
    /// succeeded or was decoded elsewhere (Progressive is decoded by the handler).
    pub error: Option<&'a str>,
}

pub(crate) fn dump(rec: &DumpRecord<'_>) {
    let Some(dir) = dir() else {
        return;
    };
    let (counter, cap) = if rec.codec == "clearcodec" {
        (&CLEAR_N, clear_cap())
    } else {
        (&PROG_N, prog_cap())
    };
    if counter.fetch_add(1, Ordering::Relaxed) >= cap {
        return;
    }
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let file = match rec.context_id {
        Some(ctx) => format!(
            "{seq:04}_{}_s{}_w{}_h{}_ctx{}.bin",
            rec.codec, rec.surface_id, rec.width, rec.height, ctx
        ),
        None => format!(
            "{seq:04}_{}_s{}_w{}_h{}.bin",
            rec.codec, rec.surface_id, rec.width, rec.height
        ),
    };
    let _ = fs::write(dir.join(&file), rec.data);

    let ctx_json = rec.context_id.map_or_else(|| "null".to_owned(), |c| c.to_string());
    let err_json = rec
        .error
        .map_or_else(|| "null".to_owned(), |e| format!("\"{}\"", json_escape(e)));
    let line = format!(
        "{{\"seq\":{seq},\"file\":\"{file}\",\"codec\":\"{}\",\"surface\":{},\"width\":{},\"height\":{},\
         \"context_id\":{ctx_json},\"caps\":\"{}\",\"bytes\":{},\"error\":{err_json}}}\n",
        rec.codec,
        rec.surface_id,
        rec.width,
        rec.height,
        json_escape(rec.caps),
        rec.data.len(),
    );
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("meta.jsonl"))
    {
        let _ = f.write_all(line.as_bytes());
    }
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}
