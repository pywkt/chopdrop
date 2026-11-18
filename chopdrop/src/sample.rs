//! Decoded audio: what a sample file looks like once the audio thread can
//! play it, and everything that can go wrong on the way.
//!
//! [`SampleData`] is the decoded sample — a rate, one or two channels and
//! interleaved `f32`; [`decode`] is how file bytes become one, through
//! symphonia; [`read_file`] is how a path becomes those bytes; and
//! [`DecodeError`] is the whole rejection taxonomy with the sentence each
//! case shows on its lane. Nothing here starts a thread or touches
//! nice-plug, so it stays testable without the framework: running a decode
//! in the background and publishing the result is `loader.rs`'s job, and
//! sharing it with the audio thread is `model.rs`'s (`SampleRef`).
//!
//! # Nothing here trusts its input
//!
//! Every byte that reaches this module is hostile until proven otherwise: it
//! came from a file manager drag or from a project file someone else saved.
//! So decoding never panics — every symphonia error is mapped, none
//! unwrapped — and the two things a 40-byte header can ask for that a plain
//! decode would grant are refused up front:
//!
//! - more memory than we are willing to spend on one sample, bounded twice:
//!   [`MAX_SAMPLE_BYTES`] on the file as read, before any decoding, and
//!   [`MAX_DECODED_FRAMES`] on what comes out of it, which is what stops a
//!   decompression bomb — a kilobyte of FLAC or Vorbis can legitimately
//!   declare hours of audio;
//! - a Bitwig `.bwclip`, which is not audio at all: its payload is
//!   compressed or encrypted, so it gets its own message rather than the
//!   baffling "unsupported audio file" a probe failure would give.

use std::borrow::Cow;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// The most channels a [`SampleData`] ever holds.
///
/// The plugin's output is stereo and a voice's pan is a stereo balance, so
/// there is nothing a third channel could be played into: anything wider is
/// folded down at load time by [`decode`] rather than carried around and
/// ignored.
pub const MAX_CHANNELS: u8 = 2;

/// The biggest file this plugin will read as a sample: 64 MiB.
///
/// Shared with persistence, which saves a sample over the cap path-only
/// instead of embedding it. 64 MiB is about six minutes of 24-bit stereo at
/// 48 kHz — far beyond anything a drum lane wants, and small enough that a
/// stray drag onto the plugin (a video, a disk image) is refused rather than
/// read.
///
/// The cap is on the file as it is on disk, checked before any decoding, so
/// nothing is spent on a file that is already too big to consider.
pub const MAX_SAMPLE_BYTES: usize = 64 * 1024 * 1024;

/// The most frames one decoded sample may hold: 2^26, about 25 minutes at
/// 44.1 kHz.
///
/// The second half of the memory bound, and the one that matters for
/// compressed formats: a few hundred bytes of FLAC, Vorbis or MP3 can
/// declare hours of audio, which [`MAX_SAMPLE_BYTES`] alone would wave
/// through and the decode would then try to hold in RAM as `f32`. At this
/// limit one sample costs at most 512 MiB decoded in stereo — the point
/// where refusing is kinder than swapping.
pub const MAX_DECODED_FRAMES: usize = 1 << 26;

/// The caps one read-and-decode is held to: bytes on disk, checked before
/// decoding, and frames decoded, checked against the header's claim and
/// again as the frames arrive.
///
/// A lane's sample uses [`SAMPLE_LIMITS`]; the chop editor's source, held
/// only while open and never embedded, uses the roomier [`SOURCE_LIMITS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most bytes a file (or an in-memory buffer) may hold.
    pub max_bytes: usize,
    /// The most frames the decoded audio may hold.
    pub max_frames: usize,
}

/// v1's caps, the ones [`read_file`] and [`decode`] apply.
pub const SAMPLE_LIMITS: Limits =
    Limits { max_bytes: MAX_SAMPLE_BYTES, max_frames: MAX_DECODED_FRAMES };

/// The biggest file the chop editor opens as a source: 512 MiB.
///
/// A source is a whole recording to chop up, not a drum hit, and it is never
/// embedded in a project, so the 64 MiB sample cap does not apply. Its length
/// is capped in seconds once the rate is known (the chop editor's
/// `MAX_SOURCE_SECONDS`, refused as [`DecodeError::SourceTooLong`]).
pub const MAX_SOURCE_BYTES: usize = 512 * 1024 * 1024;

/// The chop editor's source caps: [`MAX_SOURCE_BYTES`] and the same decoded
/// ceiling as a sample.
pub const SOURCE_LIMITS: Limits =
    Limits { max_bytes: MAX_SOURCE_BYTES, max_frames: MAX_DECODED_FRAMES };

/// What a chop-editor source past its seconds cap says.
pub const SOURCE_TOO_LONG: &str = "source too long (15 min max)";

/// What a Bitwig `.bwclip` starts with.
///
/// Dragging a clip out of Bitwig's own browser is an easy mistake to make —
/// the factory drum packages are full of them, sitting next to the plain
/// `.wav` files that do work.
const BITWIG_CLIP_MAGIC: &[u8; 4] = b"BtWg";

/// How loud the channels past the first two come back in the stereo fold.
///
/// There is no one right downmix, so this is one rule, documented and
/// tested: **the first two channels keep their own level, and the mean of
/// all the others is added to both sides at this gain** — so a 5.1 file's
/// front pair sounds exactly as loud as the same material in a stereo file,
/// while its centre and surrounds stay audible instead of vanishing. −6 dB,
/// because a fold that added them at unity would make a centre-heavy file
/// lurch forward.
///
/// The fold can push a frame past ±1.0; nothing clamps it. A sample is
/// headroom-free only at the output, and clamping here would turn a loud
/// surround file into a distorted one with no way back — lowering the lane's
/// volume is the fix, and it still works because the peak survived.
const FOLD_GAIN: f32 = 0.5;

/// A decoded sample, ready for the audio thread.
///
/// Published inside a `basedrop::Shared` and from then on read-only: the
/// audio thread only ever indexes [`SampleData::frames`], so nothing here
/// is interior-mutable or atomic.
///
/// [`rate`](Self::rate) is the file's own sample rate, kept as it was found.
/// No resampling happens at load time — playback resamples on the fly, which
/// is the same code path that has to exist for tuning anyway.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleData {
    /// The file's sample rate in Hz. Never zero: [`decode`] rejects a stream
    /// that claims no rate, because the playback step divides by it.
    pub rate: u32,
    /// 1 or 2, never more and never zero (see [`MAX_CHANNELS`]).
    pub channels: u8,
    /// The audio, interleaved: frame *n* channel *c* is
    /// `frames[n * channels + c]`. Length is always a whole number of
    /// frames, and never zero — an empty decode is an error, not a sample.
    pub frames: Box<[f32]>,
}

impl SampleData {
    /// How many frames long the sample is.
    pub fn frame_count(&self) -> usize {
        self.frames.len() / self.channels as usize
    }
}

/// Why a sample could not be loaded.
///
/// One case per way a load can fail, each carrying the short, plain sentence
/// the lane shows. The message belongs to the error rather than to the GUI
/// so that the loader, the editor and the tests all say the same thing, and
/// so that a headless restore records the same words the editor would have
/// drawn.
///
/// `Copy` and `'static` on purpose: an error travels from the loader thread
/// through the model to the GUI, and a lane keeps it until a later load
/// replaces it. Nothing here owns a path or a symphonia error — the lane
/// already knows which file it was, and a user cannot act on a codec's own
/// wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// The file could not be read: it is gone, it is not a file, or the
    /// permissions say no. Includes a project restoring a sample whose path
    /// has since moved.
    Unreadable,
    /// The file is larger than the [`Limits::max_bytes`] it was checked
    /// against ([`MAX_SAMPLE_BYTES`] for a lane, [`MAX_SOURCE_BYTES`] for a
    /// chop-editor source), which it carries so the message can name it.
    /// Refused before it is read, let alone decoded.
    TooBig { max_bytes: usize },
    /// A Bitwig `.bwclip`: the first bytes are [`BITWIG_CLIP_MAGIC`]. Not a
    /// failure to decode audio — there is no audio in there to decode.
    BitwigClip,
    /// The bytes are not audio this build can decode: probing found no
    /// format, the track's codec has no decoder, or the stream is broken
    /// enough that decoding cannot continue.
    Unsupported,
    /// The bytes decoded, but there is nothing to play: no audio track, no
    /// channels, or zero frames.
    NoAudio,
    /// The audio runs past [`MAX_DECODED_FRAMES`] — whether the container
    /// says so up front or the decode discovers it. A long recording rather
    /// than a sample, or a deliberate decompression bomb; the lane cannot
    /// tell the difference and neither can we.
    TooLong,
    /// A chop-editor source runs past its seconds cap (the chop editor's
    /// `MAX_SOURCE_SECONDS`), applied once the rate is known.
    SourceTooLong,
}

impl DecodeError {
    /// The sentence to show on the lane. Borrowed for every case but
    /// [`TooBig`](Self::TooBig), which prints its cap in whole MiB.
    pub fn message(&self) -> Cow<'static, str> {
        Cow::Borrowed(match self {
            DecodeError::Unreadable => "can't read file",
            DecodeError::TooBig { max_bytes } => {
                return Cow::Owned(format!("file too big (max {} MiB)", max_bytes >> 20));
            }
            DecodeError::BitwigClip => "Bitwig clips can't be loaded — drop a .wav instead",
            DecodeError::Unsupported => "unsupported audio file",
            DecodeError::NoAudio => "no audio in file",
            DecodeError::TooLong => "audio too long",
            DecodeError::SourceTooLong => SOURCE_TOO_LONG,
        })
    }
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for DecodeError {}

/// Read a sample file into memory, or say why not.
///
/// The path half of the taxonomy: everything that can fail before there are
/// any bytes to decode. The size cap is checked against the file's own
/// metadata first, so a 4 GiB file costs one `stat` rather than 4 GiB of
/// reading, and again on what was actually read, in case the file grew
/// between the two (a download in progress, a render still being written).
///
/// Anything that is not a plain file — a directory, a device, a socket — is
/// [`DecodeError::Unreadable`]: `/dev/urandom` is readable in every sense
/// except the one that matters.
///
/// Called from the loader thread, never from the audio or GUI thread: this
/// blocks on a disk.
pub fn read_file(path: &Path) -> Result<Vec<u8>, DecodeError> {
    read_file_with(path, &SAMPLE_LIMITS)
}

/// [`read_file`] under the given [`Limits`] (only `max_bytes` applies here):
/// [`SOURCE_LIMITS`] for a chop-editor source, or a tiny cap in a test that
/// wants the over-cap branch without a 64 MiB file on disk.
pub fn read_file_with(path: &Path, limits: &Limits) -> Result<Vec<u8>, DecodeError> {
    let max_bytes = limits.max_bytes;
    let file = File::open(path).map_err(|_| DecodeError::Unreadable)?;
    let metadata = file.metadata().map_err(|_| DecodeError::Unreadable)?;
    if !metadata.is_file() {
        return Err(DecodeError::Unreadable);
    }
    if metadata.len() > max_bytes as u64 {
        return Err(DecodeError::TooBig { max_bytes });
    }

    // One byte past the cap, so a file that grew is caught rather than
    // silently truncated into a shorter sample than the user dropped.
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max_bytes as u64 + 1).read_to_end(&mut bytes).map_err(|_| DecodeError::Unreadable)?;
    if bytes.len() > max_bytes {
        return Err(DecodeError::TooBig { max_bytes });
    }

    Ok(bytes)
}

/// Decode a whole audio file, already in memory, into [`SampleData`].
///
/// `extension` is the dropped file's extension, if it had one, passed to
/// symphonia as a hint; probing works without it (every format in scope has
/// a usable magic) but the hint saves it guessing and gets the right reader
/// first for files whose extension and contents agree.
///
/// The bytes are borrowed, not copied: a 64 MiB drop is big enough that
/// handing symphonia a second copy of it would be worth noticing.
///
/// The decode loop is the one from symphonia 0.6's own
/// `basic-interleaved` example, with the project's error policy on top:
/// individual broken packets are skipped (that is what the official loop
/// does, and it is how a slightly damaged MP3 still plays), while a stream
/// that cannot be read on at all is an [`DecodeError::Unsupported`] rather
/// than a partial sample, so a truncated file is reported instead of
/// quietly playing short.
///
/// Bytes that must not reach a decoder at all are refused first, by size and
/// by magic, which is the same screening whether they came off a disk
/// through [`read_file`] or out of a restored project's embedded base64.
pub fn decode(bytes: &[u8], extension: Option<&str>) -> Result<SampleData, DecodeError> {
    decode_with(bytes, extension, &SAMPLE_LIMITS)
}

/// [`decode`] under the given [`Limits`]: [`SOURCE_LIMITS`] for a chop-editor
/// source, or a few hundred frames in a test that wants the bomb-guard
/// branches without 2^26 of them.
pub fn decode_with(
    bytes: &[u8],
    extension: Option<&str>,
    limits: &Limits,
) -> Result<SampleData, DecodeError> {
    let max_frames = limits.max_frames;
    if bytes.len() > limits.max_bytes {
        return Err(DecodeError::TooBig { max_bytes: limits.max_bytes });
    }
    if bytes.starts_with(BITWIG_CLIP_MAGIC) {
        return Err(DecodeError::BitwigClip);
    }

    let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes)), Default::default());

    let mut hint = Hint::new();
    if let Some(extension) = extension {
        hint.with_extension(extension);
    }

    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|_| DecodeError::Unsupported)?;

    // The default audio track, and a decoder for it. Both borrows of
    // `format` end here, before the loop reads packets out of it.
    let (track_id, mut decoder) = {
        let track = format.default_track(TrackType::Audio).ok_or(DecodeError::NoAudio)?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|params| params.audio())
            .ok_or(DecodeError::Unsupported)?;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(|_| DecodeError::Unsupported)?;
        // What the container claims the track is worth, if it claims
        // anything: the cheap half of the bomb guard, since a header is read
        // long before the frames it promises are decoded.
        if track.num_frames.is_some_and(|frames| frames > max_frames as u64) {
            return Err(DecodeError::TooLong);
        }
        (track.id, decoder)
    };

    // The sample rate and channel count come from the decoded audio itself
    // rather than from the codec parameters: that is what the samples in
    // hand actually are, and some formats only fill the parameters in once
    // the first packet has been read.
    let mut rate = 0;
    let mut source_channels = 0;
    let mut frames: Vec<f32> = Vec::new();
    let mut packet_frames: Vec<f32> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            // End of the stream. In 0.6 that is `Ok(None)`, never an error.
            Ok(None) => break,
            // A chained OGG stream: the tracks change from here on, so the
            // first stream is the sample and the rest is ignored.
            Err(SymphoniaError::ResetRequired) => break,
            Err(_) => return Err(DecodeError::Unsupported),
        };

        if packet.track_id != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(audio) => {
                let spec = audio.spec();
                if rate == 0 {
                    rate = spec.rate();
                    source_channels = spec.channels().count();
                } else if spec.rate() != rate || spec.channels().count() != source_channels {
                    // A sample is one rate and one channel layout. Nothing
                    // in scope does this without `ResetRequired` first.
                    return Err(DecodeError::Unsupported);
                }
                audio.copy_to_vec_interleaved(&mut packet_frames);
                append_folded(&mut frames, &packet_frames, source_channels);
                // The other half of the bomb guard, for a stream that never
                // said how long it was (or lied): stop at the cap instead of
                // decoding on until the allocator gives up.
                if frames.len() > max_frames.saturating_mul(out_channels(source_channels)) {
                    return Err(DecodeError::TooLong);
                }
            }
            // Per the official loop: a packet that will not decode is
            // dropped and the stream carries on.
            Err(SymphoniaError::DecodeError(_)) | Err(SymphoniaError::IoError(_)) => continue,
            Err(_) => return Err(DecodeError::Unsupported),
        }
    }

    let channels = out_channels(source_channels);
    if channels == 0 || frames.is_empty() {
        return Err(DecodeError::NoAudio);
    }
    if rate == 0 {
        return Err(DecodeError::Unsupported);
    }

    Ok(SampleData { rate, channels: channels as u8, frames: frames.into_boxed_slice() })
}

/// How many channels a `source_channels`-wide file comes out as, given the
/// channel policy: itself if mono or stereo, folded to stereo if wider.
fn out_channels(source_channels: usize) -> usize {
    source_channels.min(MAX_CHANNELS as usize)
}

/// Append interleaved `source` frames, `source_channels` wide, to `dst` as
/// one or two channels — the channel policy, in one place.
///
/// Mono stays mono and stereo stays stereo, byte for byte. Wider than
/// stereo folds to stereo per [`FOLD_GAIN`]. A partial frame at the end of
/// `source` cannot happen (symphonia interleaves whole frames) and is
/// ignored rather than guessed at.
fn append_folded(dst: &mut Vec<f32>, source: &[f32], source_channels: usize) {
    match source_channels {
        0 => {}
        1 | 2 => dst.extend_from_slice(source),
        channels => {
            let rest_scale = FOLD_GAIN / (channels - 2) as f32;
            for frame in source.chunks_exact(channels) {
                let rest = frame[2..].iter().sum::<f32>() * rest_scale;
                dst.push(frame[0] + rest);
                dst.push(frame[1] + rest);
            }
        }
    }
}

/// The full scale of a 24-bit sample as [`encode_wav24`] writes it: 2^23 − 1,
/// so that +1.0 and −1.0 land on mirror-image codes.
const WAV24_SCALE: f32 = 8_388_607.0;

/// A decoded sample as a canonical 24-bit PCM WAV file — how a chop becomes
/// bytes a lane can embed and a file manager can open.
///
/// The 44-byte RIFF header (`fmt ` chunk with format tag 1, then `data`)
/// carries `data.channels` and `data.rate` as they are. Each sample is
/// clamped to [−1, 1] (NaN becomes silence) and scaled by 2^23 − 1 with
/// round-to-nearest, so −1.0 is −8388607 and −8388608 is never written: the
/// mapping is symmetric. An odd-length `data` chunk (mono, odd frame count)
/// gets the RIFF pad byte, counted in the RIFF size but not the chunk's.
///
/// Reading it back with [`decode`] divides by 2^23, so a round trip is off
/// by at most half a code of rounding plus `|x|` codes of scale — exact to
/// 2^-23 for any value already on the 24-bit grid.
pub fn encode_wav24(data: &SampleData) -> Vec<u8> {
    let channels = u16::from(data.channels);
    let block_align = channels * 3;
    let byte_rate = data.rate.saturating_mul(u32::from(block_align));
    let data_len = u32::try_from(data.frames.len() * 3).unwrap_or(u32::MAX);
    let pad = data_len % 2;
    let mut out = Vec::with_capacity(44 + data.frames.len() * 3 + pad as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&36u32.saturating_add(data_len).saturating_add(pad).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&data.rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for &sample in data.frames.iter() {
        // `clamp` passes NaN through, so NaN is caught first and made silence.
        let clamped = if sample.is_nan() { 0.0 } else { sample.clamp(-1.0, 1.0) };
        let code = (clamped * WAV24_SCALE).round() as i32;
        out.extend_from_slice(&code.to_le_bytes()[..3]);
    }
    if pad == 1 {
        out.push(0);
    }
    out
}

// `pub(crate)` only so that `state.rs`'s round-trip test can borrow the WAV
// generator below rather than keep a second copy of a RIFF header: that
// test wants real generated-WAV bytes. Nothing outside a test build can
// see this.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// WAVE format tag 1: integer PCM.
    const WAVE_FORMAT_PCM: u16 = 1;
    /// WAVE format tag 3: IEEE float PCM.
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

    /// A canonical 44-byte-header WAV around already-encoded sample bytes.
    ///
    /// The WAV test bytes are built in code, which keeps the fixtures
    /// readable and the expected output arithmetic rather than recorded.
    fn wav(format_tag: u16, bits: u16, channels: u16, rate: u32, data: &[u8]) -> Vec<u8> {
        let block_align = channels * bits / 8;
        let byte_rate = rate * u32::from(block_align);
        let mut out = Vec::with_capacity(44 + data.len());
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&format_tag.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&block_align.to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    /// A 16-bit WAV of exactly these interleaved samples. Shared with
    /// `state.rs`'s tests (see this module's own comment).
    pub(crate) fn wav_i16(channels: u16, rate: u32, samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        wav(WAVE_FORMAT_PCM, 16, channels, rate, &data)
    }

    /// A 32-bit float WAV of exactly these interleaved samples.
    fn wav_f32(channels: u16, rate: u32, samples: &[f32]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        wav(WAVE_FORMAT_IEEE_FLOAT, 32, channels, rate, &data)
    }

    /// An 8-bit WAV. Integer PCM at 8 bits is the one WAVE depth that is
    /// *unsigned*: 128 is silence, 0 is the negative extreme.
    fn wav_u8(channels: u16, rate: u32, samples: &[u8]) -> Vec<u8> {
        wav(WAVE_FORMAT_PCM, 8, channels, rate, samples)
    }

    /// A 24-bit WAV: the low three bytes of each sample, little-endian.
    fn wav_i24(channels: u16, rate: u32, samples: &[i32]) -> Vec<u8> {
        let data: Vec<u8> = samples
            .iter()
            .flat_map(|s| {
                let bytes = s.to_le_bytes();
                [bytes[0], bytes[1], bytes[2]]
            })
            .collect();
        wav(WAVE_FORMAT_PCM, 24, channels, rate, &data)
    }

    /// A 32-bit integer WAV (not the float one — same width, different tag).
    fn wav_i32(channels: u16, rate: u32, samples: &[i32]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        wav(WAVE_FORMAT_PCM, 32, channels, rate, &data)
    }

    /// An AIFF-C compression type: uncompressed 32-bit big-endian float.
    const AIFC_FLOAT32: &[u8; 4] = b"fl32";

    /// A sample rate as AIFF's 80-bit IEEE 754 extended float: a sign bit, a
    /// 15-bit exponent biased by 16383, then 64 mantissa bits *including* the
    /// leading one (unlike the 32- and 64-bit formats, which imply it).
    ///
    /// Only whole rates are needed, which keeps this exact: shift the value
    /// up until its top set bit lands on bit 63 and the exponent says by how
    /// much.
    fn extended_rate(rate: u32) -> [u8; 10] {
        let leading_zeros = rate.leading_zeros();
        let mut out = [0u8; 10];
        out[..2].copy_from_slice(&(16_383 + 31 - leading_zeros as u16).to_be_bytes());
        out[2..].copy_from_slice(&(u64::from(rate) << (32 + leading_zeros)).to_be_bytes());
        out
    }

    /// Append one big-endian RIFF chunk: tag, length, body, and the pad byte
    /// that keeps the next chunk on an even offset.
    fn be_chunk(out: &mut Vec<u8>, tag: &[u8; 4], body: &[u8]) {
        out.extend_from_slice(tag);
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(body);
        if body.len() % 2 == 1 {
            out.push(0);
        }
    }

    /// A minimal AIFF (or AIFF-C, if `compression` is given) around
    /// already-encoded, big-endian sample bytes: the `COMM` chunk that
    /// describes them and the `SSND` chunk that holds them.
    ///
    /// The AIFF test bytes are built in code like the WAV ones. The
    /// difference worth knowing is that AIFF is big-endian throughout and
    /// that float samples are not plain AIFF at all: they need the AIFF-C
    /// form, whose `COMM` carries a compression type and a (here empty)
    /// Pascal-string name after the rate.
    fn aiff(
        compression: Option<&[u8; 4]>,
        bits: u16,
        channels: u16,
        rate: u32,
        data: &[u8],
    ) -> Vec<u8> {
        let frame_bytes = u32::from(channels) * u32::from(bits) / 8;
        let mut comm = Vec::new();
        comm.extend_from_slice(&channels.to_be_bytes());
        comm.extend_from_slice(&(data.len() as u32 / frame_bytes).to_be_bytes());
        comm.extend_from_slice(&bits.to_be_bytes());
        comm.extend_from_slice(&extended_rate(rate));
        if let Some(compression) = compression {
            comm.extend_from_slice(compression);
            // An empty Pascal string: a zero length byte, then its pad byte.
            comm.extend_from_slice(&[0, 0]);
        }

        let mut ssnd = Vec::with_capacity(8 + data.len());
        ssnd.extend_from_slice(&0u32.to_be_bytes()); // offset: none
        ssnd.extend_from_slice(&0u32.to_be_bytes()); // block size: unaligned
        ssnd.extend_from_slice(data);

        let mut body = Vec::new();
        body.extend_from_slice(if compression.is_some() { b"AIFC" } else { b"AIFF" });
        be_chunk(&mut body, b"COMM", &comm);
        be_chunk(&mut body, b"SSND", &ssnd);

        let mut out = Vec::with_capacity(8 + body.len());
        out.extend_from_slice(b"FORM");
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// A 16-bit AIFF of exactly these interleaved samples.
    fn aiff_i16(channels: u16, rate: u32, samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_be_bytes()).collect();
        aiff(None, 16, channels, rate, &data)
    }

    /// A 24-bit AIFF: the high three bytes of each sample, big-endian.
    fn aiff_i24(channels: u16, rate: u32, samples: &[i32]) -> Vec<u8> {
        let data: Vec<u8> = samples
            .iter()
            .flat_map(|s| {
                let bytes = s.to_be_bytes();
                [bytes[1], bytes[2], bytes[3]]
            })
            .collect();
        aiff(None, 24, channels, rate, &data)
    }

    /// A 32-bit float AIFF-C of exactly these interleaved samples.
    fn aifc_f32(channels: u16, rate: u32, samples: &[f32]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_be_bytes()).collect();
        aiff(Some(AIFC_FLOAT32), 32, channels, rate, &data)
    }

    /// The checked-in fixtures for the three formats that cannot be written
    /// by hand (see `chopdrop/tests/fixtures/README.md` for how they were
    /// made).
    ///
    /// The FLAC one is lossless and carries [`FIXTURE_PATTERN`] exactly; the
    /// two lossy ones carry 0.1 s of a half-scale 440 Hz sine and are
    /// asserted loosely.
    const FLAC_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/tiny_mono.flac");
    const MP3_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/tiny_mono.mp3");
    const OGG_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/tiny_stereo.ogg");

    /// The 44100 Hz mono fixtures are this long: 0.1 s for the lossy pair.
    const FIXTURE_RATE: u32 = 44_100;
    const LOSSY_FIXTURE_FRAMES: usize = 4_410;

    /// What the FLAC fixture holds, frame for frame: 400 frames of a
    /// shallow 16-bit ramp. Recomputed here rather than recorded, so the
    /// assertion fails if the fixture is ever regenerated differently.
    fn flac_fixture_samples() -> Vec<i16> {
        (0..400).map(|n| (n - 200) * 80).collect()
    }

    /// Peak and RMS of an interleaved buffer — the two numbers the lossy
    /// fixtures are judged by, since their samples are not reproducible.
    fn peak_and_rms(frames: &[f32]) -> (f32, f32) {
        let peak = frames.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        let rms = (frames.iter().map(|s| s * s).sum::<f32>() / frames.len() as f32).sqrt();
        (peak, rms)
    }

    /// A few hundred frames of a known pattern: a ramp walking the whole
    /// 16-bit range, so the scaling test sees both signs and the extremes.
    fn ramp_i16(count: usize) -> Vec<i16> {
        (0..count)
            .map(|n| (i16::MIN as i32 + (n as i32 * 65_535 / (count as i32 - 1))) as i16)
            .collect()
    }

    /// Limits with only the byte cap and frame cap given — the synthetic
    /// caps the `_with` tests use instead of 64 MiB files and 2^26 frames.
    fn frames_and_bytes(max_bytes: usize, max_frames: usize) -> Limits {
        Limits { max_bytes, max_frames }
    }

    /// [`SAMPLE_LIMITS`] with a smaller frame cap.
    fn frame_cap(max_frames: usize) -> Limits {
        Limits { max_frames, ..SAMPLE_LIMITS }
    }

    /// The whole point of the module: the samples come out exactly as the
    /// file put them in, at the file's own rate.
    ///
    /// This also **pins symphonia's integer scaling** (`i16 / 32768.0`): the
    /// expected values are computed here, not read back from the decoder, so
    /// an upgrade that changed the conversion — to `/ 32767.0`, say — fails
    /// this test instead of quietly changing how every 16-bit sample sounds.
    #[test]
    fn a_16_bit_wav_decodes_to_exactly_its_samples() {
        let samples = ramp_i16(400);
        let decoded = decode(&wav_i16(1, 44_100, &samples), Some("wav")).unwrap();

        assert_eq!(decoded.rate, 44_100);
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.frame_count(), 400);

        let expected: Vec<f32> = samples.iter().map(|s| f32::from(*s) / 32_768.0).collect();
        assert_eq!(decoded.frames.as_ref(), expected.as_slice());
    }

    /// The scaling, said once more in whole numbers, so the pin is readable
    /// without running the ramp arithmetic in your head.
    #[test]
    fn the_16_bit_scaling_is_a_division_by_32768() {
        let samples = [0i16, 1, -1, i16::MAX, i16::MIN, 16_384];
        let decoded = decode(&wav_i16(1, 48_000, &samples), None).unwrap();

        assert_eq!(
            decoded.frames.as_ref(),
            [0.0, 1.0 / 32_768.0, -1.0 / 32_768.0, 32_767.0 / 32_768.0, -1.0, 0.5]
        );
    }

    /// A float WAV needs no conversion at all, so every value survives bit
    /// for bit — including the ones outside ±1.0 that a float file is
    /// allowed to carry.
    #[test]
    fn a_float_wav_decodes_to_exactly_its_samples() {
        let samples: Vec<f32> = (0..300)
            .map(|n| (n as f32 / 300.0) * 2.4 - 1.2)
            .chain([0.0, 1.0, -1.0, 1.75, -1.75])
            .collect();
        let decoded = decode(&wav_f32(1, 22_050, &samples), Some("wav")).unwrap();

        assert_eq!(decoded.rate, 22_050);
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.frames.as_ref(), samples.as_slice());
    }

    /// Stereo stays stereo, in interleaved order, with the channels the
    /// right way round.
    #[test]
    fn a_stereo_wav_keeps_both_channels_interleaved() {
        // Left counts up, right counts down: a swap cannot hide.
        let samples = [100i16, -100, 200, -200, 300, -300, 400, -400];
        let decoded = decode(&wav_i16(2, 44_100, &samples), Some("wav")).unwrap();

        assert_eq!(decoded.channels, 2);
        assert_eq!(decoded.frame_count(), 4);
        let expected: Vec<f32> = samples.iter().map(|s| f32::from(*s) / 32_768.0).collect();
        assert_eq!(decoded.frames.as_ref(), expected.as_slice());
    }

    /// The channel policy, on its own, without a file: the first two
    /// channels keep their level and the mean of the rest joins both sides
    /// at [`FOLD_GAIN`].
    #[test]
    fn wider_than_stereo_folds_the_extra_channels_into_both_sides() {
        let mut out = Vec::new();
        // One 4-channel frame: L, R, then two others averaging 0.375.
        // Every value here is exact in binary, so the fold can be asserted
        // to the bit rather than to a tolerance.
        append_folded(&mut out, &[1.0, -1.0, 0.5, 0.25], 4);
        assert_eq!(out, [1.0 + 0.1875, -1.0 + 0.1875]);

        // Six channels — a 5.1 file — average all four of the others.
        let mut out = Vec::new();
        append_folded(&mut out, &[1.0, -1.0, 0.2, 0.4, 0.6, 0.8], 6);
        let rest = (0.2 + 0.4 + 0.6 + 0.8) / 4.0 * 0.5;
        assert!((out[0] - (1.0 + rest)).abs() < 1e-6, "{out:?}");
        assert!((out[1] - (-1.0 + rest)).abs() < 1e-6, "{out:?}");

        // Mono and stereo pass through untouched.
        let mut out = Vec::new();
        append_folded(&mut out, &[0.1, 0.2, 0.3], 1);
        assert_eq!(out, [0.1, 0.2, 0.3]);
        let mut out = Vec::new();
        append_folded(&mut out, &[0.1, 0.2, 0.3, 0.4], 2);
        assert_eq!(out, [0.1, 0.2, 0.3, 0.4]);

        // A channel count of zero appends nothing rather than dividing by it.
        let mut out = Vec::new();
        append_folded(&mut out, &[0.1, 0.2], 0);
        assert!(out.is_empty());
    }

    /// And the same policy through a real file, so the fold is wired into
    /// the decode and not just unit-tested beside it.
    #[test]
    fn a_three_channel_wav_decodes_to_stereo() {
        // Two frames: L, R, C. C folds into both sides at half level.
        let samples = [8_192i16, -8_192, 4_096, 0, 0, 16_384];
        let decoded = decode(&wav_i16(3, 44_100, &samples), Some("wav")).unwrap();

        assert_eq!(decoded.channels, 2);
        assert_eq!(decoded.frame_count(), 2);
        assert_eq!(decoded.frames.as_ref(), [0.25 + 0.0625, -0.25 + 0.0625, 0.25, 0.25]);
    }

    /// Hostile input is the normal case here (drops, restored projects), so
    /// rubbish has to come back as an error — never a panic, and never an
    /// empty sample that playback would then index into.
    #[test]
    fn bytes_that_are_not_audio_are_rejected() {
        assert_eq!(decode(b"", None), Err(DecodeError::Unsupported));
        assert_eq!(decode(b"not audio at all", Some("wav")), Err(DecodeError::Unsupported));
        // A WAV header with no samples behind it: readable, but nothing to play.
        assert_eq!(decode(&wav_i16(1, 44_100, &[]), Some("wav")), Err(DecodeError::NoAudio));
        // A truncated WAV: the header promises 400 frames, 10 bytes follow.
        let mut truncated = wav_i16(1, 44_100, &ramp_i16(400));
        truncated.truncate(54);
        assert!(decode(&truncated, Some("wav")).is_err());
    }

    /// 8-bit WAV, the odd one out: unsigned, so the conversion is an offset
    /// as well as a scale. Pinned the same way as the 16-bit one.
    #[test]
    fn an_8_bit_wav_decodes_to_exactly_its_samples() {
        let samples = [0u8, 1, 127, 128, 129, 255];
        let decoded = decode(&wav_u8(1, 8_000, &samples), Some("wav")).unwrap();

        assert_eq!(decoded.rate, 8_000);
        assert_eq!(decoded.channels, 1);
        let expected: Vec<f32> =
            samples.iter().map(|s| (f32::from(*s) - 128.0) / 128.0).collect();
        assert_eq!(decoded.frames.as_ref(), expected.as_slice());
    }

    /// 24-bit WAV — the depth most sample libraries ship. Scaling is a
    /// division by 2^23.
    #[test]
    fn a_24_bit_wav_decodes_to_exactly_its_samples() {
        let samples = [0i32, 1, -1, 8_388_607, -8_388_608, 4_194_304];
        let decoded = decode(&wav_i24(1, 48_000, &samples), Some("wav")).unwrap();

        assert_eq!(decoded.rate, 48_000);
        assert_eq!(decoded.channels, 1);
        let expected: Vec<f32> = samples.iter().map(|s| *s as f32 / 8_388_608.0).collect();
        assert_eq!(decoded.frames.as_ref(), expected.as_slice());
    }

    /// 32-bit integer WAV: the same width as the float one but a different
    /// format tag, and a division by 2^31.
    #[test]
    fn a_32_bit_integer_wav_decodes_to_exactly_its_samples() {
        let samples = [0i32, 1, -1, i32::MAX, i32::MIN, 1 << 30];
        let decoded = decode(&wav_i32(1, 96_000, &samples), Some("wav")).unwrap();

        assert_eq!(decoded.rate, 96_000);
        assert_eq!(decoded.channels, 1);
        let expected: Vec<f32> = samples.iter().map(|s| *s as f32 / 2_147_483_648.0).collect();
        assert_eq!(decoded.frames.as_ref(), expected.as_slice());
    }

    /// AIFF, 16-bit: a different container and the opposite byte order, same
    /// samples out. Mono in, mono out — the first half of the channel policy.
    #[test]
    fn a_16_bit_aiff_decodes_to_exactly_its_samples() {
        let samples = ramp_i16(400);
        let decoded = decode(&aiff_i16(1, 44_100, &samples), Some("aiff")).unwrap();

        assert_eq!(decoded.rate, 44_100);
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.frame_count(), 400);
        let expected: Vec<f32> = samples.iter().map(|s| f32::from(*s) / 32_768.0).collect();
        assert_eq!(decoded.frames.as_ref(), expected.as_slice());
    }

    /// AIFF, 24-bit.
    #[test]
    fn a_24_bit_aiff_decodes_to_exactly_its_samples() {
        let samples = [0i32, 1, -1, 8_388_607, -8_388_608, 4_194_304];
        let decoded = decode(&aiff_i24(1, 48_000, &samples), Some("aiff")).unwrap();

        assert_eq!(decoded.rate, 48_000);
        assert_eq!(decoded.channels, 1);
        let expected: Vec<f32> = samples.iter().map(|s| *s as f32 / 8_388_608.0).collect();
        assert_eq!(decoded.frames.as_ref(), expected.as_slice());
    }

    /// AIFF-C, 32-bit float: no conversion, so values outside ±1.0 survive
    /// here too.
    #[test]
    fn a_float_aiff_decodes_to_exactly_its_samples() {
        let samples: Vec<f32> = (0..300)
            .map(|n| (n as f32 / 300.0) * 2.4 - 1.2)
            .chain([0.0, 1.0, -1.0, 1.75, -1.75])
            .collect();
        let decoded = decode(&aifc_f32(1, 22_050, &samples), Some("aiff")).unwrap();

        assert_eq!(decoded.rate, 22_050);
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.frames.as_ref(), samples.as_slice());
    }

    /// Stereo AIFF keeps both channels, the right way round.
    #[test]
    fn a_stereo_aiff_keeps_both_channels_interleaved() {
        let samples = [100i16, -100, 200, -200, 300, -300];
        let decoded = decode(&aiff_i16(2, 44_100, &samples), Some("aiff")).unwrap();

        assert_eq!(decoded.channels, 2);
        assert_eq!(decoded.frame_count(), 3);
        let expected: Vec<f32> = samples.iter().map(|s| f32::from(*s) / 32_768.0).collect();
        assert_eq!(decoded.frames.as_ref(), expected.as_slice());
    }

    /// The other half of the channel policy, through AIFF: four channels
    /// fold to two, the front pair at its own level and the rest at
    /// [`FOLD_GAIN`]. (The WAV path is covered by
    /// `a_three_channel_wav_decodes_to_stereo`; AIFF reaches it through a
    /// different reader, which describes >3 channels as discrete.)
    #[test]
    fn a_four_channel_aiff_folds_to_stereo() {
        // Frame one: L and R at ±0.25, then two others averaging 0.0625,
        // which joins both sides at half that. Frame two: silence but for
        // one surround channel at 0.5, so half of its own half — 0.125 —
        // is all that is left of the frame.
        let samples = [8_192i16, -8_192, 4_096, 0, 0, 0, 0, 16_384];
        let decoded = decode(&aiff_i16(4, 44_100, &samples), Some("aiff")).unwrap();

        assert_eq!(decoded.channels, 2);
        assert_eq!(decoded.frame_count(), 2);
        assert_eq!(decoded.frames.as_ref(), [0.25 + 0.03125, -0.25 + 0.03125, 0.125, 0.125]);
    }

    /// FLAC, from the checked-in fixture. Lossless, so this is an exact
    /// assertion like the WAV and AIFF ones: a decoder change that altered
    /// the samples by a bit would fail here.
    #[test]
    fn the_flac_fixture_decodes_to_exactly_its_samples() {
        let decoded = decode(FLAC_FIXTURE, Some("flac")).unwrap();

        assert_eq!(decoded.rate, FIXTURE_RATE);
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.frame_count(), 400);
        let expected: Vec<f32> =
            flac_fixture_samples().iter().map(|s| f32::from(*s) / 32_768.0).collect();
        assert_eq!(decoded.frames.as_ref(), expected.as_slice());
    }

    /// MP3, from the checked-in fixture. Lossy, so the assertion is on the
    /// shape of the result rather than its samples: the right rate and
    /// channel count, a length within a few MP3 granules of the original,
    /// and a half-scale sine's peak and RMS.
    #[test]
    fn the_mp3_fixture_decodes_to_roughly_its_samples() {
        let decoded = decode(MP3_FIXTURE, Some("mp3")).unwrap();

        assert_eq!(decoded.rate, FIXTURE_RATE);
        assert_eq!(decoded.channels, 1);
        // An MP3 granule is 1152 frames, and encoder delay and padding are
        // trimmed or not depending on the decoder's gapless support.
        let slack = decoded.frame_count().abs_diff(LOSSY_FIXTURE_FRAMES);
        assert!(slack <= 2 * 1152, "frames {} far off", decoded.frame_count());

        let (peak, rms) = peak_and_rms(&decoded.frames);
        assert!((0.4..0.6).contains(&peak), "peak {peak}");
        // 0.5 / sqrt(2) for a half-scale sine, with room for a quiet lead-in.
        assert!((0.30..0.40).contains(&rms), "rms {rms}");
    }

    /// OGG/Vorbis, from the checked-in fixture — and the stereo half of the
    /// channel policy through a lossy format: the fixture's right channel is
    /// the inverse of its left, so a reader that merged, swapped or dropped
    /// a channel could not pass.
    #[test]
    fn the_ogg_vorbis_fixture_decodes_to_roughly_its_samples_in_stereo() {
        let decoded = decode(OGG_FIXTURE, Some("ogg")).unwrap();

        assert_eq!(decoded.rate, FIXTURE_RATE);
        assert_eq!(decoded.channels, 2);
        let slack = decoded.frame_count().abs_diff(LOSSY_FIXTURE_FRAMES);
        assert!(slack <= 2 * 1024, "frames {} far off", decoded.frame_count());

        let (peak, rms) = peak_and_rms(&decoded.frames);
        assert!((0.4..0.6).contains(&peak), "peak {peak}");
        assert!((0.30..0.40).contains(&rms), "rms {rms}");

        for frame in decoded.frames.as_chunks::<2>().0 {
            assert!((frame[0] + frame[1]).abs() < 0.05, "channels not opposed: {frame:?}");
        }
    }

    /// Every format in scope probes from its contents alone: a file the user
    /// renamed, or dropped without an extension, still loads.
    #[test]
    fn every_format_is_recognised_without_an_extension_hint() {
        let wav = wav_i16(1, 44_100, &ramp_i16(400));
        let aiff = aiff_i16(1, 44_100, &ramp_i16(400));
        for (name, bytes) in [
            ("wav", wav.as_slice()),
            ("aiff", aiff.as_slice()),
            ("flac", FLAC_FIXTURE),
            ("mp3", MP3_FIXTURE),
            ("ogg", OGG_FIXTURE),
        ] {
            let decoded = decode(bytes, None).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(decoded.rate, 44_100, "{name}");
            assert!(decoded.frame_count() > 0, "{name}");
        }
    }

    /// A WAV whose `data` chunk header claims `declared_bytes` of audio
    /// whatever it actually holds: a decompression bomb in the simplest
    /// container there is, and the one case where the claim alone is enough
    /// to refuse. The size field sits at 40..44 of the canonical header.
    fn wav_declaring(declared_bytes: u32, samples: &[i16]) -> Vec<u8> {
        let mut out = wav_i16(1, 44_100, samples);
        out[40..44].copy_from_slice(&declared_bytes.to_le_bytes());
        // The RIFF length (4..8) has to cover the chunk it contains, or the
        // reader refuses the file as malformed before reading the claim.
        out[4..8].copy_from_slice(&declared_bytes.saturating_add(36).to_le_bytes());
        out
    }

    /// The FLAC fixture with its STREAMINFO total-sample count zeroed,
    /// which is how FLAC says "unknown length" (a live stream, an encoder
    /// still running). Nothing then tells the decoder how much audio is
    /// coming until it has decoded it.
    ///
    /// Layout: `fLaC`, a 4-byte metadata block header, then STREAMINFO —
    /// 10 bytes of block and frame sizes, then a big-endian u64 whose top
    /// 28 bits are rate/channels/depth and whose low 36 bits are the total
    /// sample count. So: clear the low 36 bits of the u64 at offset 18.
    /// The MD5 that follows is of the audio, not of this field, and
    /// symphonia does not check it.
    fn flac_of_unknown_length() -> Vec<u8> {
        let mut out = FLAC_FIXTURE.to_vec();
        let header = u64::from_be_bytes(out[18..26].try_into().unwrap());
        out[18..26].copy_from_slice(&(header & !((1u64 << 36) - 1)).to_be_bytes());
        out
    }

    /// A path in the temp directory that no other test uses.
    fn scratch_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("chopdrop-{}-{name}", std::process::id()))
    }

    /// Rejection, case by case: the wording is part of the contract — the
    /// lane shows it, and the loader and a headless restore record it — so
    /// it is pinned here rather than left to whoever next edits the enum.
    #[test]
    fn every_rejection_has_the_message_the_spec_gives_it() {
        assert_eq!(DecodeError::Unreadable.message(), "can't read file");
        let sample_cap = DecodeError::TooBig { max_bytes: SAMPLE_LIMITS.max_bytes };
        assert_eq!(sample_cap.message(), "file too big (max 64 MiB)");
        let source_cap = DecodeError::TooBig { max_bytes: SOURCE_LIMITS.max_bytes };
        assert_eq!(source_cap.message(), "file too big (max 512 MiB)");
        assert_eq!(
            DecodeError::BitwigClip.message(),
            "Bitwig clips can't be loaded — drop a .wav instead"
        );
        assert_eq!(DecodeError::Unsupported.message(), "unsupported audio file");
        assert_eq!(DecodeError::NoAudio.message(), "no audio in file");
        assert_eq!(DecodeError::TooLong.message(), "audio too long");
        assert_eq!(DecodeError::SourceTooLong.message(), "source too long (15 min max)");
        assert_eq!(SOURCE_TOO_LONG, DecodeError::SourceTooLong.message());
    }

    /// Every rejection says something short a user can act on, and says it
    /// the same way through `Display` as through [`DecodeError::message`].
    #[test]
    fn every_error_has_a_short_plain_message() {
        for error in [
            DecodeError::Unreadable,
            DecodeError::TooBig { max_bytes: MAX_SAMPLE_BYTES },
            DecodeError::TooBig { max_bytes: MAX_SOURCE_BYTES },
            DecodeError::BitwigClip,
            DecodeError::Unsupported,
            DecodeError::NoAudio,
            DecodeError::TooLong,
            DecodeError::SourceTooLong,
        ] {
            let message = error.message();
            assert!(!message.is_empty() && message.len() < 60, "{message:?}");
            assert_eq!(error.to_string(), message);
        }
    }

    /// The message names the cap the file was checked against, read from
    /// the error rather than written into the text: a lane's 64 MiB and a
    /// chop-editor source's 512 MiB, raised by both `_with` functions.
    #[test]
    fn the_cap_in_the_message_is_the_cap_in_the_code() {
        assert_eq!(MAX_SAMPLE_BYTES, 64 * 1024 * 1024);
        assert_eq!(MAX_SOURCE_BYTES, 512 * 1024 * 1024);

        let path = scratch_path("over-both-caps.wav");
        let file = File::create(&path).expect("temp dir is writable");
        file.set_len(MAX_SOURCE_BYTES as u64 + 1).expect("sparse file");
        drop(file);
        let as_sample = read_file_with(&path, &SAMPLE_LIMITS);
        let as_source = read_file_with(&path, &SOURCE_LIMITS);
        std::fs::remove_file(&path).expect("clean up");

        let as_sample = as_sample.unwrap_err();
        assert_eq!(as_sample, DecodeError::TooBig { max_bytes: MAX_SAMPLE_BYTES });
        assert_eq!(as_sample.message(), "file too big (max 64 MiB)");
        let as_source = as_source.unwrap_err();
        assert_eq!(as_source, DecodeError::TooBig { max_bytes: MAX_SOURCE_BYTES });
        assert_eq!(as_source.message(), "file too big (max 512 MiB)");

        // The bytes half says the same, cap for cap (only the length is
        // checked first, so a zeroed buffer one past the sample cap does).
        let oversized = vec![0u8; MAX_SAMPLE_BYTES + 1];
        let rejected = decode_with(&oversized, None, &SAMPLE_LIMITS).unwrap_err();
        assert_eq!(rejected.message(), "file too big (max 64 MiB)");
        let tiny = frames_and_bytes(16, MAX_DECODED_FRAMES);
        let rejected = decode_with(&oversized[..17], None, &tiny).unwrap_err();
        assert_eq!(rejected, DecodeError::TooBig { max_bytes: 16 });
    }

    /// A path that is not there: the common case, since a project can be
    /// restored on a machine where the sample never was.
    #[test]
    fn a_missing_file_cannot_be_read() {
        let missing = scratch_path("no-such-sample.wav");
        assert_eq!(read_file(&missing), Err(DecodeError::Unreadable));
        assert_eq!(read_file(&missing).unwrap_err().message(), "can't read file");
    }

    /// A directory opens and stats perfectly well and is still not a
    /// sample. Nor is a device or a socket — the same branch covers them.
    #[test]
    fn a_directory_is_not_a_sample() {
        assert_eq!(read_file(&std::env::temp_dir()), Err(DecodeError::Unreadable));
    }

    /// Over the cap: refused from the file's own size, before a byte of it
    /// is read. The file here is sparse — it costs no disk — which is only
    /// possible *because* nothing reads it.
    #[test]
    fn a_file_over_the_cap_is_refused_before_it_is_read() {
        let path = scratch_path("too-big.wav");
        let file = File::create(&path).expect("temp dir is writable");
        file.set_len(MAX_SAMPLE_BYTES as u64 + 1).expect("sparse file");
        drop(file);

        let rejected = read_file(&path);
        std::fs::remove_file(&path).expect("clean up");

        assert_eq!(rejected, Err(DecodeError::TooBig { max_bytes: MAX_SAMPLE_BYTES }));
        assert_eq!(rejected.unwrap_err().message(), "file too big (max 64 MiB)");
    }

    /// A file that reads longer than it measures is still capped. `stat`
    /// is a promise, not a fact: a file can grow between the two syscalls
    /// (a download in flight), and `/proc` files report no size at all
    /// while reading several kilobytes.
    #[test]
    fn a_file_that_reads_longer_than_it_measures_is_refused() {
        let status = std::path::Path::new("/proc/self/status");
        assert_eq!(std::fs::metadata(status).unwrap().len(), 0, "procfs reports no size");
        let tiny = frames_and_bytes(16, MAX_DECODED_FRAMES);
        assert_eq!(read_file_with(status, &tiny), Err(DecodeError::TooBig { max_bytes: 16 }));
    }

    /// The same cap applies to bytes that never came from a path: a
    /// restored project's embedded sample is just as untrusted, and is
    /// screened before any decoder sees it.
    #[test]
    fn bytes_over_the_cap_are_refused_without_decoding() {
        let oversized = vec![0u8; MAX_SAMPLE_BYTES + 1];
        let too_big = DecodeError::TooBig { max_bytes: MAX_SAMPLE_BYTES };
        assert_eq!(decode(&oversized, Some("wav")), Err(too_big));
    }

    /// A Bitwig clip gets its own sentence. Dropping one is an easy
    /// mistake — Bitwig's factory drum packages are full of them — and
    /// "unsupported audio file" would send the user looking for a codec
    /// that cannot exist.
    #[test]
    fn a_bitwig_clip_is_named_as_one() {
        let mut clip = b"BtWg".to_vec();
        clip.extend_from_slice(&[0x42; 64]);

        let rejected = decode(&clip, Some("bwclip"));
        assert_eq!(rejected, Err(DecodeError::BitwigClip));
        assert_eq!(
            rejected.unwrap_err().message(),
            "Bitwig clips can't be loaded — drop a .wav instead"
        );
        // The magic decides, not the extension: a clip renamed `.wav` is
        // still a clip, and a `.wav` that really is one still loads.
        assert_eq!(decode(&clip, Some("wav")), Err(DecodeError::BitwigClip));
        assert!(decode(&wav_i16(1, 44_100, &ramp_i16(8)), Some("bwclip")).is_ok());
        // Four bytes that merely start the same way are not a clip; they
        // are not audio either, which is a different message.
        assert_eq!(decode(b"BtW", None), Err(DecodeError::Unsupported));
    }

    /// A container that declares more audio than the cap allows is refused
    /// from its header alone: 40-odd bytes claiming two billion frames
    /// cost nothing to refuse and would cost 8 GiB to humour.
    #[test]
    fn a_container_that_declares_too_much_audio_is_refused() {
        let bomb = wav_declaring(4_000_000_000, &ramp_i16(8));
        assert!(bomb.len() < 100, "the claim is in the header, not the file");

        let rejected = decode(&bomb, Some("wav"));
        assert_eq!(rejected, Err(DecodeError::TooLong));
        assert_eq!(rejected.unwrap_err().message(), "audio too long");
    }

    /// A stream that declares no length at all is bounded by the decode
    /// itself: the frames are counted as they arrive and the cap stops
    /// them. This is the branch a real compressed bomb takes — a few
    /// hundred bytes of FLAC or Vorbis that never say how long they are.
    #[test]
    fn a_stream_of_unknown_length_is_still_bounded() {
        let unknown = flac_of_unknown_length();
        // The patch leaves a perfectly good 400-frame FLAC: under the real
        // cap it decodes exactly as the untouched fixture does, so the
        // rejection below is the cap talking and not a broken file.
        let decoded = decode(&unknown, Some("flac")).expect("still valid FLAC");
        assert_eq!(decoded.frame_count(), 400);
        assert_eq!(decoded, decode(FLAC_FIXTURE, Some("flac")).unwrap());

        assert_eq!(decode_with(&unknown, Some("flac"), &frame_cap(100)), Err(DecodeError::TooLong));
        // One frame under is enough to refuse; the limit itself is not.
        assert_eq!(decode_with(&unknown, Some("flac"), &frame_cap(399)), Err(DecodeError::TooLong));
        assert!(decode_with(&unknown, Some("flac"), &frame_cap(400)).is_ok());
    }

    /// `decode_with` holds a file to the frame cap it is given, and the cap
    /// itself is allowed: 101 frames are refused at 100 and accepted at 101,
    /// whether the header says so (WAV) or only the decode finds out.
    #[test]
    fn decode_with_refuses_one_frame_over_the_cap_and_accepts_the_cap() {
        let wav = wav_i16(1, 44_100, &ramp_i16(101));
        assert_eq!(decode_with(&wav, Some("wav"), &frame_cap(100)), Err(DecodeError::TooLong));
        let accepted = decode_with(&wav, Some("wav"), &frame_cap(101)).expect("at the cap");
        assert_eq!(accepted.frame_count(), 101);
        assert_eq!(accepted, decode(&wav, Some("wav")).unwrap());

        let stereo = wav_i16(2, 48_000, &ramp_i16(202));
        assert_eq!(decode_with(&stereo, None, &frame_cap(100)), Err(DecodeError::TooLong));
        assert_eq!(decode_with(&stereo, None, &frame_cap(101)).unwrap().frame_count(), 101);
    }

    /// `decode_with`'s byte cap is the one it is given, not the sample cap:
    /// bytes one past it are refused before any decoding, at it they decode.
    #[test]
    fn decode_with_applies_its_own_byte_cap() {
        let wav = wav_i16(1, 44_100, &ramp_i16(101));
        let tight = frames_and_bytes(wav.len() - 1, MAX_DECODED_FRAMES);
        let too_big = DecodeError::TooBig { max_bytes: wav.len() - 1 };
        assert_eq!(decode_with(&wav, Some("wav"), &tight), Err(too_big));
        let exact = frames_and_bytes(wav.len(), MAX_DECODED_FRAMES);
        assert_eq!(decode_with(&wav, Some("wav"), &exact).unwrap().frame_count(), 101);
    }

    /// `read_file_with` uses its own byte cap, and `read_file` is exactly
    /// `read_file_with(SAMPLE_LIMITS)`.
    #[test]
    fn read_file_with_applies_its_own_byte_cap() {
        let wav = wav_i16(1, 44_100, &ramp_i16(101));
        let dir = std::env::temp_dir().join(format!("chopdrop-limits-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("101.wav");
        std::fs::write(&path, &wav).unwrap();

        let tight = frames_and_bytes(wav.len() - 1, MAX_DECODED_FRAMES);
        let too_big = DecodeError::TooBig { max_bytes: wav.len() - 1 };
        assert_eq!(read_file_with(&path, &tight), Err(too_big));
        let exact = frames_and_bytes(wav.len(), MAX_DECODED_FRAMES);
        assert_eq!(read_file_with(&path, &exact).as_deref(), Ok(&wav[..]));
        assert_eq!(read_file(&path), read_file_with(&path, &SAMPLE_LIMITS));
        assert_eq!(read_file_with(&path, &SOURCE_LIMITS).as_deref(), Ok(&wav[..]));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The limits are the intended numbers.
    #[test]
    fn the_limits_are_the_specs() {
        assert_eq!(SAMPLE_LIMITS, Limits { max_bytes: 64 << 20, max_frames: 1 << 26 });
        assert_eq!(MAX_SOURCE_BYTES, 512 * 1024 * 1024);
        assert_eq!(SOURCE_LIMITS, Limits { max_bytes: 512 << 20, max_frames: 1 << 26 });
    }

    /// The decoded cap counts frames, not samples: a stereo file is not
    /// half as long as the same audio in mono.
    #[test]
    fn the_decoded_cap_counts_frames_not_samples() {
        let stereo = wav_i16(2, 44_100, &ramp_i16(200));
        assert_eq!(decode(&stereo, Some("wav")).unwrap().frame_count(), 100);
        assert_eq!(decode_with(&stereo, Some("wav"), &frame_cap(99)), Err(DecodeError::TooLong));
        assert!(decode_with(&stereo, Some("wav"), &frame_cap(100)).is_ok());
    }

    /// Nothing in the taxonomy panics, whatever the bytes say: the whole
    /// point of the enum is that a hostile file comes back as a message.
    #[test]
    fn hostile_headers_come_back_as_errors_not_panics() {
        let cases: Vec<Vec<u8>> = vec![
            wav_declaring(u32::MAX, &ramp_i16(4)),
            wav_declaring(u32::MAX, &[]),
            wav_declaring(0, &ramp_i16(4)),
            flac_of_unknown_length()[..20].to_vec(),
            b"BtWg".to_vec(),
            vec![0u8; 1024],
            FLAC_FIXTURE[..FLAC_FIXTURE.len() / 2].to_vec(),
            MP3_FIXTURE[..MP3_FIXTURE.len() / 2].to_vec(),
            OGG_FIXTURE[..OGG_FIXTURE.len() / 2].to_vec(),
        ];
        for bytes in cases {
            // Either outcome is fine; not returning is not.
            let _ = decode(&bytes, None);
            let _ = decode(&bytes, Some("wav"));
            let _ = decode_with(&bytes, Some("flac"), &frame_cap(1));
        }
    }

    /// A little-endian header field, read back.
    fn le_u16(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([bytes[at], bytes[at + 1]])
    }

    fn le_u32(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    }

    /// A deterministic, full-range test signal: a sine sweep with a few
    /// extremes mixed in, `len` samples long.
    fn wav24_signal(len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| match i % 7 {
                0 => 1.0,
                3 => -1.0,
                5 => 0.0,
                _ => (i as f32 * 0.37).sin() * 0.999,
            })
            .collect()
    }

    fn sample_of(channels: u8, rate: u32, frames: Vec<f32>) -> SampleData {
        SampleData { rate, channels, frames: frames.into_boxed_slice() }
    }

    #[test]
    fn encode_wav24_writes_the_canonical_header() {
        let data = sample_of(2, 44_100, wav24_signal(2 * 10));
        let bytes = encode_wav24(&data);

        assert_eq!(bytes.len(), 44 + 10 * 2 * 3);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(le_u32(&bytes, 4), 36 + 60);
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(le_u32(&bytes, 16), 16);
        assert_eq!(le_u16(&bytes, 20), 1, "PCM format tag");
        assert_eq!(le_u16(&bytes, 22), 2, "channels");
        assert_eq!(le_u32(&bytes, 24), 44_100, "rate");
        assert_eq!(le_u32(&bytes, 28), 44_100 * 6, "byte rate");
        assert_eq!(le_u16(&bytes, 32), 6, "block align");
        assert_eq!(le_u16(&bytes, 34), 24, "bits per sample");
        assert_eq!(&bytes[36..40], b"data");
        assert_eq!(le_u32(&bytes, 40), 60, "data chunk length");
    }

    #[test]
    fn encode_wav24_pads_an_odd_data_chunk() {
        let data = sample_of(1, 48_000, wav24_signal(5));
        let bytes = encode_wav24(&data);

        assert_eq!(le_u16(&bytes, 22), 1);
        assert_eq!(le_u32(&bytes, 28), 48_000 * 3);
        assert_eq!(le_u16(&bytes, 32), 3);
        assert_eq!(le_u32(&bytes, 40), 15, "the chunk length excludes the pad");
        assert_eq!(le_u32(&bytes, 4), 36 + 15 + 1, "the RIFF length includes it");
        assert_eq!(bytes.len(), 44 + 15 + 1);
        assert_eq!(bytes[bytes.len() - 1], 0);
    }

    #[test]
    fn encode_wav24_maps_the_extremes_symmetrically_and_clamps() {
        let data = sample_of(1, 48_000, vec![1.0, -1.0, 7.5, -3.0, f32::NAN, 0.5]);
        let bytes = encode_wav24(&data);
        let codes: Vec<i32> = bytes[44..44 + 18]
            .as_chunks::<3>()
            .0
            .iter()
            .map(|b| i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8)
            .collect();

        assert_eq!(codes, [8_388_607, -8_388_607, 8_388_607, -8_388_607, 0, 4_194_304]);
        let decoded = decode(&bytes, Some("wav")).unwrap();
        assert_eq!(decoded.frames[2], decoded.frames[0], "an over-range float clamps to +1");
        assert_eq!(decoded.frames[3], decoded.frames[1], "an under-range float clamps to -1");
    }

    /// `decode(encode(x))` for mono, stereo and odd frame counts, on the
    /// 24-bit grid (`k / (2^23 - 1)`): there the only error is the
    /// encoder-vs-decoder scale, at most `|x| * 2^-23`, so within 2^-23.
    #[test]
    fn encode_wav24_round_trips_on_grid_within_one_code() {
        for (channels, frame_count) in [(1u8, 64usize), (1, 101), (2, 64), (2, 101)] {
            let frames: Vec<f32> = wav24_signal(frame_count * channels as usize)
                .iter()
                .map(|x| (x * WAV24_SCALE).round() / WAV24_SCALE)
                .collect();
            let data = sample_of(channels, 32_000, frames);
            let decoded = decode(&encode_wav24(&data), Some("wav")).unwrap();

            assert_eq!(decoded.rate, 32_000);
            assert_eq!(decoded.channels, channels);
            assert_eq!(decoded.frame_count(), frame_count);
            for (x, y) in data.frames.iter().zip(decoded.frames.iter()) {
                assert!((x - y).abs() <= 2f32.powi(-23), "{channels}ch {frame_count}: {x} vs {y}");
            }
        }
    }

    /// Off the grid, rounding adds up to half a code to the scale error.
    #[test]
    fn encode_wav24_round_trips_any_value_within_a_code_and_a_half() {
        for (channels, frame_count) in [(1u8, 77usize), (2, 77)] {
            let data = sample_of(channels, 44_100, wav24_signal(frame_count * channels as usize));
            let decoded = decode(&encode_wav24(&data), Some("wav")).unwrap();

            assert_eq!(decoded.frame_count(), frame_count);
            for (x, y) in data.frames.iter().zip(decoded.frames.iter()) {
                assert!((x - y).abs() <= 1.5 * 2f32.powi(-23), "{x} vs {y}");
            }
        }
    }
}
