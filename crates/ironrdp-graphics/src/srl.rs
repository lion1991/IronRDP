//! SRL (Simplified Run-Length) entropy codec for progressive upgrade passes.
//!
//! Faithful port of FreeRDP `progressive_rfx_srl_read` (libfreerdp/codec/progressive.c).
//! The decoder is **stateful** and threaded across all subbands of one component:
//! a single SRL bit stream feeds every band, carrying adaptive `kp`, the pending
//! zero-run `nz`, and the `mode` flag between reads. Recreating the reader per band
//! (or resetting `kp`) desyncs the whole stream, so callers must build one
//! [`SrlDecoder`] per component and call [`SrlDecoder::read`] once per zero-DAS
//! coefficient in band order.
//!
//! Magnitudes use capped-unary coding: `mag` starts at 1 and counts 0-bits until a
//! 1-bit or `mag == (1 << numBits) - 1`.

/// Stateful SRL decoder over one component's SRL byte stream.
///
/// Mirrors FreeRDP's `RFX_PROGRESSIVE_UPGRADE_STATE` SRL half (`kp`/`nz`/`mode`).
pub struct SrlDecoder<'a> {
    bits: BitReader<'a>,
    kp: i32,
    nz: i32,
    mode: bool,
}

impl<'a> SrlDecoder<'a> {
    /// Attach a decoder to a component's SRL stream. `kp` starts at 8 (FreeRDP).
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            bits: BitReader::new(data),
            kp: 8,
            nz: 0,
            mode: false,
        }
    }

    /// Read one coefficient value (`0` if it stays zero this pass).
    ///
    /// `num_bits` is the band's refinement bit width. Faithful port of
    /// `progressive_rfx_srl_read`.
    #[expect(clippy::similar_names, reason = "mag/max are standard SRL magnitude names")]
    pub fn read(&mut self, num_bits: u8) -> i16 {
        if self.nz > 0 {
            self.nz -= 1;
            return 0;
        }

        let k = u32::try_from(self.kp / 8).unwrap_or(0);

        if !self.mode {
            // zero encoding
            let bit = self.bits.read_bit();
            if !bit {
                // '0' bit: run of exactly (1 << k) zeros
                self.nz = 1i32.checked_shl(k).unwrap_or(i32::MAX);
                self.kp += 4;
                if self.kp > 80 {
                    self.kp = 80;
                }
                self.nz -= 1;
                return 0;
            }
            // '1' bit: unary encoding follows; nz = next k bits
            self.nz = 0;
            self.mode = true;
            if k > 0 {
                self.nz = i32::try_from(self.bits.read_bits(k)).unwrap_or(0);
            }
            if self.nz > 0 {
                self.nz -= 1;
                return 0;
            }
        }

        // unary (value) encoding
        self.mode = false;
        let sign = self.bits.read_bit();

        if self.kp < 6 {
            self.kp = 0;
        } else {
            self.kp -= 6;
        }

        if num_bits <= 1 {
            return if sign { -1 } else { 1 };
        }

        let max = 1u32.checked_shl(u32::from(num_bits)).unwrap_or(0).wrapping_sub(1);
        let mut mag = 1u32;
        while mag < max {
            if self.bits.read_bit() {
                break;
            }
            mag += 1;
        }

        let mag = mag.min(i16::MAX.unsigned_abs().into());
        let mag = i16::try_from(mag).unwrap_or(i16::MAX);
        if sign {
            -mag
        } else {
            mag
        }
    }
}

/// Stateful SRL encoder: exact inverse of [`SrlDecoder`] over one stream.
///
/// Buffers pending zero runs until a non-zero value flushes them. Trailing zeros
/// need no encoding — the decoder's past-end reads produce zeros. Encode-side only
/// (server/tests); not on the real decode path.
pub struct SrlEncoder {
    w: BitWriter,
    kp: i32,
    pending_zeros: u32,
}

impl SrlEncoder {
    pub fn new() -> Self {
        Self {
            w: BitWriter::new(),
            kp: 8,
            pending_zeros: 0,
        }
    }

    /// Encode one coefficient value (`0` accumulates into the pending zero run).
    #[expect(clippy::similar_names, reason = "mag/max are standard SRL magnitude names")]
    pub fn write(&mut self, value: i16, num_bits: u8) {
        if value == 0 {
            self.pending_zeros += 1;
            return;
        }
        self.flush_zeros();

        // value (unary) encoding
        let sign = value < 0;
        self.w.write_bit(sign);
        if self.kp < 6 {
            self.kp = 0;
        } else {
            self.kp -= 6;
        }
        if num_bits <= 1 {
            return;
        }

        let max = (1u32 << num_bits) - 1;
        let mag = u32::from(value.unsigned_abs()).clamp(1, max);
        // capped-unary: (mag-1) zeros, then a terminating 1 unless mag == max
        for _ in 1..mag {
            self.w.write_bit(false);
        }
        if mag < max {
            self.w.write_bit(true);
        }
    }

    fn flush_zeros(&mut self) {
        let mut z = self.pending_zeros;
        loop {
            let k = u32::try_from(self.kp / 8).unwrap_or(0);
            let chunk = 1u32.checked_shl(k).unwrap_or(u32::MAX);
            if z >= chunk {
                self.w.write_bit(false);
                self.kp += 4;
                if self.kp > 80 {
                    self.kp = 80;
                }
                z -= chunk;
            } else {
                self.w.write_bit(true);
                self.w.write_bits(z, k);
                break;
            }
        }
        self.pending_zeros = 0;
    }

    /// Finish the stream, dropping trailing zeros (decoder fills them past-end).
    pub fn finish(self) -> Vec<u8> {
        self.w.finish()
    }
}

impl Default for SrlEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Encode a standalone value sequence (single stream). Test/helper convenience.
pub fn encode_srl(values: &[i16], num_bits: u8) -> Vec<u8> {
    let mut enc = SrlEncoder::new();
    for &v in values {
        enc.write(v, num_bits);
    }
    enc.finish()
}

// ---------------------------------------------------------------------------
// Bit-level I/O helpers
// ---------------------------------------------------------------------------

struct BitReader<'a> {
    data: &'a [u8],
    byte_idx: usize,
    bit_idx: u8, // 0..7, MSB first
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_idx: 0,
            bit_idx: 0,
        }
    }

    /// Read one bit MSB-first; past end-of-stream reads as `false` (FreeRDP's
    /// bit stream shifts in zeros).
    fn read_bit(&mut self) -> bool {
        if self.byte_idx >= self.data.len() {
            return false;
        }
        let bit = (self.data[self.byte_idx] >> (7 - self.bit_idx)) & 1 != 0;
        self.bit_idx += 1;
        if self.bit_idx >= 8 {
            self.bit_idx = 0;
            self.byte_idx += 1;
        }
        bit
    }

    fn read_bits(&mut self, count: u32) -> u32 {
        let mut value = 0u32;
        for _ in 0..count {
            value = (value << 1) | u32::from(self.read_bit());
        }
        value
    }
}

struct BitWriter {
    bytes: Vec<u8>,
    current: u8,
    bit_count: u8, // bits written in current byte (0..7)
}

impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            current: 0,
            bit_count: 0,
        }
    }

    fn write_bit(&mut self, bit: bool) {
        self.current = (self.current << 1) | u8::from(bit);
        self.bit_count += 1;
        if self.bit_count >= 8 {
            self.bytes.push(self.current);
            self.current = 0;
            self.bit_count = 0;
        }
    }

    fn write_bits(&mut self, value: u32, count: u32) {
        for i in (0..count).rev() {
            self.write_bit((value >> i) & 1 != 0);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bit_count > 0 {
            // Pad remaining bits with zeros (MSB aligned)
            self.current <<= 8 - self.bit_count;
            self.bytes.push(self.current);
        }
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode `n` values from a fresh single-stream decoder.
    fn decode_seq(data: &[u8], n: usize, num_bits: u8) -> Vec<i16> {
        let mut d = SrlDecoder::new(data);
        core::iter::repeat_with(|| d.read(num_bits)).take(n).collect()
    }

    #[test]
    fn round_trip_single_positive() {
        let v = vec![1];
        assert_eq!(decode_seq(&encode_srl(&v, 1), 1, 1), v);
    }

    #[test]
    fn round_trip_single_negative() {
        let v = vec![-1];
        assert_eq!(decode_seq(&encode_srl(&v, 1), 1, 1), v);
    }

    #[test]
    fn round_trip_mixed_zeros() {
        // Leading/interior zeros and multi-bit magnitudes.
        let v = vec![0, 0, 1, -1, 0, 3];
        assert_eq!(decode_seq(&encode_srl(&v, 4), v.len(), 4), v);
    }

    #[test]
    fn round_trip_nonzero_only() {
        let v = vec![1, -1, 2, -3, 1];
        assert_eq!(decode_seq(&encode_srl(&v, 4), v.len(), 4), v);
    }

    #[test]
    fn round_trip_long_zero_runs() {
        // Exercises the adaptive kp chunking across long runs.
        let mut v = vec![0i16; 50];
        v[10] = 5;
        v[40] = -7;
        assert_eq!(decode_seq(&encode_srl(&v, 4), v.len(), 4), v);
    }

    #[test]
    fn round_trip_capped_magnitude() {
        // num_bits=3 => max magnitude = 7; values at and near the cap.
        let v = vec![7, 6, 1, -7, 4];
        assert_eq!(decode_seq(&encode_srl(&v, 3), v.len(), 3), v);
    }

    #[test]
    fn all_zeros_via_past_end() {
        // Empty/short stream: every position decodes to zero.
        assert_eq!(decode_seq(&[], 5, 3), vec![0, 0, 0, 0, 0]);
    }

    #[test]
    fn bit_reader_basic() {
        let data = [0b1011_0000];
        let mut reader = BitReader::new(&data);
        assert!(reader.read_bit());
        assert!(!reader.read_bit());
        assert!(reader.read_bit());
        assert!(reader.read_bit());
    }

    #[test]
    fn bit_writer_basic() {
        let mut writer = BitWriter::new();
        for b in [true, false, true, true, false, false, false, false] {
            writer.write_bit(b);
        }
        assert_eq!(writer.finish(), vec![0b1011_0000]);
    }
}
