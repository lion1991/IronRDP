//! NSCodec (MS-RDPNSC) bitmap stream decoder.
//!
//! Used by the ClearCodec subcodec layer ([MS-RDPEGFX] 2.2.4.1) where servers
//! encode mixed text/photo regions. Semantics ported from FreeRDP
//! `libfreerdp/codec/nsc.c`: four RLE-compressed planes (Y, Co, Cg, A) in
//! AYCoCg color space with color-loss recovery and optional 2x chroma
//! subsampling.

use ironrdp_core::{invalid_field_err, DecodeResult};

/// Decodes an NSCODEC_BITMAP_STREAM into BGRA pixels (`width * height * 4`).
pub fn decode_nscodec(data: &[u8], width: u16, height: u16) -> DecodeResult<Vec<u8>> {
    if data.len() < 20 {
        return Err(invalid_field_err!("nscodec", "stream shorter than 20-byte header"));
    }
    let mut plane_byte_count = [0usize; 4];
    for (i, c) in plane_byte_count.iter_mut().enumerate() {
        *c = u32::from_le_bytes([data[i * 4], data[i * 4 + 1], data[i * 4 + 2], data[i * 4 + 3]]) as usize;
    }
    let color_loss_level = data[16];
    if !(1..=7).contains(&color_loss_level) {
        return Err(invalid_field_err!("nscodec", "ColorLossLevel out of [1,7]"));
    }
    let subsampling = data[17] != 0;
    let total: usize = plane_byte_count.iter().sum();
    let planes_data = data
        .get(20..)
        .filter(|p| p.len() >= total)
        .ok_or_else(|| invalid_field_err!("nscodec", "plane data shorter than declared counts"))?;

    let w = usize::from(width);
    let h = usize::from(height);
    if w == 0 || h == 0 {
        return Ok(Vec::new());
    }
    // FreeRDP: luma stride rounds up to 8, chroma planes to half of the
    // 8/2-rounded dims when subsampled; alpha always tight w*h.
    let rw = w.div_ceil(8) * 8;
    let rh = h.div_ceil(2) * 2;
    let org_byte_count = if subsampling {
        [rw * h, (rw / 2) * (rh / 2), (rw / 2) * (rh / 2), w * h]
    } else {
        [w * h; 4]
    };

    let mut planes: [Vec<u8>; 4] = Default::default();
    let mut rest = planes_data;
    for i in 0..4 {
        let (chunk, tail) = rest.split_at(plane_byte_count[i]);
        rest = tail;
        planes[i] = if plane_byte_count[i] == 0 {
            // Absent plane decodes as all-0xFF (FreeRDP nsc_rle_decompress_data).
            vec![0xFF; org_byte_count[i]]
        } else if plane_byte_count[i] < org_byte_count[i] {
            rle_decode(chunk, org_byte_count[i])?
        } else {
            chunk[..org_byte_count[i]].to_vec()
        };
    }

    let shift = color_loss_level - 1;
    let (y_stride, c_stride) = if subsampling { (rw, rw / 2) } else { (w, w) };
    let mut out = vec![0u8; w * h * 4];
    for row in 0..h {
        let y_row = row * y_stride;
        let c_row = if subsampling { (row / 2) * c_stride } else { row * c_stride };
        let a_row = row * w;
        for col in 0..w {
            let c_col = if subsampling { col / 2 } else { col };
            let yv = i16::from(*planes[0].get(y_row + col).unwrap_or(&0));
            // Color-loss recovery: shift then truncate to i8 (sign via wraparound).
            let co = i16::from((i16::from(*planes[1].get(c_row + c_col).unwrap_or(&0)) << shift) as i8);
            let cg = i16::from((i16::from(*planes[2].get(c_row + c_col).unwrap_or(&0)) << shift) as i8);
            let a = *planes[3].get(a_row + col).unwrap_or(&0xFF);
            let r = (yv + co - cg).clamp(0, 255) as u8;
            let g = (yv + cg).clamp(0, 255) as u8;
            let b = (yv - co - cg).clamp(0, 255) as u8;
            let o = (row * w + col) * 4;
            out[o] = b;
            out[o + 1] = g;
            out[o + 2] = r;
            out[o + 3] = a;
        }
    }
    Ok(out)
}

/// NSCodec plane RLE ([MS-RDPNSC] 2.2.2 / FreeRDP `nsc_rle_decode`): a byte
/// followed by an equal byte starts a run (len byte + 2, or 0xFF marker + LE
/// u32); the final 4 bytes of every plane are stored raw.
fn rle_decode(mut input: &[u8], original_size: usize) -> DecodeResult<Vec<u8>> {
    let mut out = Vec::with_capacity(original_size);
    let mut left = original_size;
    while left > 4 {
        let (&value, rest) = input
            .split_first()
            .ok_or_else(|| invalid_field_err!("nscodec", "RLE input exhausted"))?;
        input = rest;
        if left == 5 {
            out.push(value);
            left -= 1;
            continue;
        }
        let &next = input
            .first()
            .ok_or_else(|| invalid_field_err!("nscodec", "RLE input exhausted"))?;
        if next != value {
            out.push(value);
            left -= 1;
            continue;
        }
        input = &input[1..];
        let &len_byte = input
            .first()
            .ok_or_else(|| invalid_field_err!("nscodec", "RLE run length missing"))?;
        let len = if len_byte < 0xFF {
            input = &input[1..];
            usize::from(len_byte) + 2
        } else {
            if input.len() < 5 {
                return Err(invalid_field_err!("nscodec", "RLE u32 run length truncated"));
            }
            let l = u32::from_le_bytes([input[1], input[2], input[3], input[4]]) as usize;
            input = &input[5..];
            l
        };
        if len > left {
            return Err(invalid_field_err!("nscodec", "RLE run exceeds plane size"));
        }
        out.resize(out.len() + len, value);
        left -= len;
    }
    if input.len() < left {
        return Err(invalid_field_err!("nscodec", "RLE raw tail truncated"));
    }
    out.extend_from_slice(&input[..left]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rle_literal_and_run() {
        // orig=10: literal 'A', run 'B'x2+len(3)=5, then 4 raw tail bytes.
        // stream: A, B, B, 3, w, x, y, z  → out: A BBBBB wxyz
        let input = [0x41, 0x42, 0x42, 3, 1, 2, 3, 4];
        let out = rle_decode(&input, 10).unwrap();
        assert_eq!(out, vec![0x41, 0x42, 0x42, 0x42, 0x42, 0x42, 1, 2, 3, 4]);
    }

    #[test]
    fn rle_tail_only() {
        // orig=4: loop never runs, 4 raw bytes.
        let out = rle_decode(&[9, 8, 7, 6], 4).unwrap();
        assert_eq!(out, vec![9, 8, 7, 6]);
    }

    #[test]
    fn rle_left_five_forces_literal() {
        // orig=5: first byte literal even if next equals (FreeRDP left==5 branch).
        let out = rle_decode(&[5, 5, 1, 2, 3], 5).unwrap();
        assert_eq!(out, vec![5, 5, 1, 2, 3]);
    }

    /// Build a raw (uncompressed planes, no subsampling) NSCodec stream.
    fn raw_stream(w: usize, h: usize, y: u8, co: u8, cg: u8, a: u8, cll: u8) -> Vec<u8> {
        let n = w * h;
        let mut s = Vec::new();
        for _ in 0..4 {
            s.extend_from_slice(&(n as u32).to_le_bytes());
        }
        s.push(cll);
        s.push(0); // no chroma subsampling
        s.extend_from_slice(&[0, 0]);
        for v in [y, co, cg, a] {
            s.extend(std::iter::repeat(v).take(n));
        }
        s
    }

    #[test]
    fn ycocg_conversion_matches_freerdp() {
        // y=100, co=10, cg=246(-10 as i8), CLL=1 (shift 0):
        // r = 100+10-(-10)=120, g = 100-10=90, b = 100-10+10=100, BGRA order.
        let s = raw_stream(4, 2, 100, 10, 246, 0x80, 1);
        let px = decode_nscodec(&s, 4, 2).unwrap();
        assert_eq!(px.len(), 4 * 2 * 4);
        for p in px.chunks_exact(4) {
            assert_eq!(p, &[100, 90, 120, 0x80]);
        }
    }

    #[test]
    fn color_loss_shift_applies() {
        // co=5, CLL=3 → shift 2 → co=20; cg=0. y=50: r=70, g=50, b=30.
        let s = raw_stream(2, 2, 50, 5, 0, 0xFF, 3);
        let px = decode_nscodec(&s, 2, 2).unwrap();
        for p in px.chunks_exact(4) {
            assert_eq!(p, &[30, 50, 70, 0xFF]);
        }
    }

    #[test]
    fn absent_planes_decode_as_ff() {
        // All plane counts 0 → planes all 0xFF: y=255, co=cg=-1 (i8 of 0xFF), shift 0:
        // r=255-1+1=255, g=254, b=255+1+1→255.
        let mut s = Vec::new();
        for _ in 0..4 {
            s.extend_from_slice(&0u32.to_le_bytes());
        }
        s.extend_from_slice(&[1, 0, 0, 0]);
        let px = decode_nscodec(&s, 2, 1).unwrap();
        for p in px.chunks_exact(4) {
            assert_eq!(p, &[255, 254, 255, 0xFF]);
        }
    }

    #[test]
    fn chroma_subsampling_strides() {
        // 2x2, subsampled: luma stride rw=8 (rounded), chroma 4x1 plane shared
        // by all pixels. Luma varies per pixel; chroma constant.
        let w = 2usize;
        let h = 2usize;
        let rw = 8usize;
        let rh = 2usize;
        let luma_n = rw * h;
        let chroma_n = (rw / 2) * (rh / 2);
        let alpha_n = w * h;
        let mut s = Vec::new();
        for n in [luma_n, chroma_n, chroma_n, alpha_n] {
            s.extend_from_slice(&(n as u32).to_le_bytes());
        }
        s.extend_from_slice(&[1, 1, 0, 0]); // CLL=1, subsampling on
        let mut luma = vec![0u8; luma_n];
        luma[0] = 10; // (0,0)
        luma[1] = 20; // (1,0)
        luma[rw] = 30; // (0,1)
        luma[rw + 1] = 40; // (1,1)
        s.extend_from_slice(&luma);
        s.extend(std::iter::repeat(0u8).take(chroma_n)); // co=0
        s.extend(std::iter::repeat(0u8).take(chroma_n)); // cg=0
        s.extend(std::iter::repeat(0xFFu8).take(alpha_n));
        let px = decode_nscodec(&s, 2, 2).unwrap();
        // co=cg=0 → r=g=b=y
        let at = |x: usize, y: usize| {
            let o = (y * w + x) * 4;
            (px[o], px[o + 1], px[o + 2])
        };
        assert_eq!(at(0, 0), (10, 10, 10));
        assert_eq!(at(1, 0), (20, 20, 20));
        assert_eq!(at(0, 1), (30, 30, 30));
        assert_eq!(at(1, 1), (40, 40, 40));
    }
}
