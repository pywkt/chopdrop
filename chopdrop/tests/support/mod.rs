//! Test audio the integration tests build in code, shared by every test
//! binary that needs file bytes a real decoder will accept.
//!
//! Not a test target of its own: Cargo compiles only the top-level files in
//! `chopdrop/tests/` as tests, so this is reached with `mod support;` from
//! each one that wants it (and the unused-code warnings are per binary,
//! which is why a test includes it only when it uses it).
//!
//! `sample.rs` keeps a generator of its own for the decoder's unit tests —
//! WAV at six depths plus AIFF — but it is `#[cfg(test)]` and so invisible
//! from here (an integration test is its own crate). This is the one copy on
//! the integration side; the plain 16-bit header below is the same one the
//! decoder is tested against there.

/// WAVE format tag 1: integer PCM.
const WAVE_FORMAT_PCM: u16 = 1;

/// Bits per sample of the one depth this generates.
const BITS: u16 = 16;

/// Bytes of header before the samples: the `RIFF` chunk, a 16-byte `fmt `
/// and the `data` header.
const HEADER_LEN: usize = 44;

/// A canonical 44-byte-header 16-bit WAV of exactly these interleaved
/// samples.
///
/// Generated rather than checked in so the audio a test expects back out of
/// the loader is arithmetic rather than recorded (the checked-in
/// `fixtures/` files are there for the formats code cannot write:
/// `chopdrop/tests/fixtures/README.md`).
pub fn wav_i16(channels: u16, rate: u32, samples: &[i16]) -> Vec<u8> {
    let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let block_align = channels * BITS / 8;
    let mut out = Vec::with_capacity(HEADER_LEN + data.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&WAVE_FORMAT_PCM.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * u32::from(block_align)).to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&BITS.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    out
}
