//! Legacy ReCycle files: `.rex` files and `.rcy` documents.
//!
//! Older ReCycle versions wrapped an AIFF (`FORM … AIFF`) and put the tempo and the slice
//! table in an `APPL` chunk signed `REX ` (exported loop) or `ReCy` (ReCycle document). The
//! audio itself is ordinary big-endian AIFF PCM in `SSND`; the `MARK` chunk usually carries
//! "Loop start" and "Loop end" markers. The slice tables are the same ones the reference
//! reader uses:
//!
//! - `REX `: magic `0xD1D1D1DA`, one 12-byte record per slice at offset `0x3F8` (start,
//!   length in frames, position in 1/16 PPQ ticks).
//! - `ReCy`: magic `0xD1DADED0`, one 8-byte record per slice at offset `0xA0` (flags and
//!   state, start, analysis points). A slice's length runs to the next slice's start, or to
//!   the loop end.
//!
//! Like the REX2 reader this one treats the file as untrusted input: every size is checked,
//! tables are bounded, and nothing panics on damaged data. The result is the same [`RexFile`]
//! the REX2 path returns, at 16-bit scale (8-bit samples are expanded, 24- and 32-bit samples
//! are reduced).

use crate::{RexFile, RexSlice, MAX_DECODED_SAMPLES, MAX_SLICES};

/// Magic at the start of the `REX ` slice table.
const REX_MAGIC: u32 = 0xd1d1_d1da;
/// Magic at the start of the `ReCy` slice table.
const RECY_MAGIC: u32 = 0xd1da_ded0;
/// Offset of the first `REX ` slice record.
const REX_TABLE: usize = 0x3f8;
/// Offset of the first `ReCy` slice record.
const RECY_TABLE: usize = 0xa0;
/// The most slice records either table may hold (the reference reader's limit).
const MAX_LEGACY_SLICES: usize = 1_000;

/// True when `data` is an AIFF container, the envelope both legacy formats use.
pub(crate) fn looks_like(data: &[u8]) -> bool {
    data.len() >= 12 && &data[0..4] == b"FORM" && &data[8..12] == b"AIFF"
}

/// Everything the legacy parser needs from the AIFF container.
#[derive(Default)]
struct Aiff<'a> {
    comm: Option<Comm>,
    ssnd: Option<&'a [u8]>,
    app: Option<&'a [u8]>,
    loop_start: Option<u32>,
    loop_end: Option<u32>,
}

struct Comm {
    channels: usize,
    frames: u32,
    bits: u16,
    rate: u32,
}

/// Parses a legacy `.rex`/`.rcy` file.
pub(crate) fn parse(data: &[u8]) -> Result<RexFile, String> {
    if !looks_like(data) {
        return Err("not a REX2 file and not an AIFF-based ReCycle file".to_string());
    }
    let aiff = parse_aiff(data)?;
    let comm = aiff.comm.ok_or("the legacy file has no `COMM` chunk")?;
    if !(1..=2).contains(&comm.channels) {
        return Err(format!(
            "the legacy file says {} channels; only 1 and 2 are known",
            comm.channels
        ));
    }
    if !matches!(comm.bits, 8 | 16 | 24 | 32) {
        return Err(format!(
            "the legacy file says {} bits per sample; 8, 16, 24 and 32 are known",
            comm.bits
        ));
    }
    if comm.rate == 0 {
        return Err("the legacy file says no sample rate".to_string());
    }
    let app = aiff
        .app
        .ok_or("not a ReCycle file: no `REX ` or `ReCy` APPL chunk")?;
    let is_recy = app.starts_with(b"ReCy");
    let is_rex = app.starts_with(b"REX ");
    if !is_recy && !is_rex {
        return Err("the legacy file has no `REX ` or `ReCy` APPL chunk".to_string());
    }
    let ssnd = aiff
        .ssnd
        .ok_or("the legacy file has no `SSND` audio chunk")?;
    if comm.frames == 0 {
        return Err("the legacy file says the waveform has no frames".to_string());
    }

    let mut warnings = Vec::new();
    let frame_bytes = usize::from(comm.bits).div_ceil(8) * comm.channels;
    let available = (ssnd.len() / frame_bytes).min(u32::MAX as usize) as u32;
    if comm.frames > available {
        warnings.push(format!(
            "the audio data holds {available} of the {} frames the file declares",
            comm.frames
        ));
    }
    let mut frames = comm.frames.min(available);
    let budget = MAX_DECODED_SAMPLES / comm.channels;
    if frames as usize > budget {
        warnings.push(format!(
            "the file declares {frames} frames; only the first {budget} are decoded"
        ));
        frames = budget as u32;
    }

    let loop_start = aiff.loop_start.filter(|&s| s < frames).unwrap_or(0);
    let loop_end = aiff
        .loop_end
        .filter(|&e| e > loop_start && e <= frames)
        .unwrap_or(frames);

    let (mut slices, markers) = if is_recy {
        recy_slices(app, frames, loop_end)?
    } else {
        rex_slices(app)?
    };
    if slices.len() > MAX_SLICES {
        warnings.push(format!(
            "more than {MAX_SLICES} slices; the rest are ignored"
        ));
        slices.truncate(MAX_SLICES);
    }
    if slices.is_empty() && markers.is_empty() {
        warnings.push("the file has no slices; the waveform is used whole".to_string());
    }
    for slice in &slices {
        if slice.end() > frames {
            warnings.push(format!(
                "slice at frame {} ends past the waveform and was shortened",
                slice.start
            ));
            break;
        }
    }

    let tempo = legacy_tempo(app, is_recy);
    if tempo.is_none() {
        warnings.push("the legacy file has no usable tempo".to_string());
    }
    match comm.bits {
        16 => {}
        8 => {
            warnings.push("the file stores 8-bit samples; they are expanded to 16-bit".to_string())
        }
        other => warnings.push(format!(
            "the file stores {other}-bit samples; the waveform is reduced to 16-bit"
        )),
    }

    let samples = decode_pcm(ssnd, comm.channels, comm.bits, frames);
    Ok(RexFile {
        tempo,
        original_tempo: tempo,
        sample_rate: comm.rate,
        channels: comm.channels as u8,
        bit_depth: comm.bits,
        frames,
        loop_start,
        loop_end,
        slices,
        markers,
        samples,
        warnings,
    })
}

fn parse_aiff(data: &[u8]) -> Result<Aiff<'_>, String> {
    let declared = usize::try_from(be32_at(data, 4).ok_or("the FORM size is missing")?)
        .map_err(|_| "the FORM size is too large".to_string())?;
    let end = declared
        .checked_add(8)
        .ok_or("the FORM size overflows")?
        .min(data.len());
    let mut out = Aiff::default();
    let mut off = 12usize;
    while off + 8 <= end {
        let id = &data[off..off + 4];
        let size = usize::try_from(be32_at(data, off + 4).ok_or("a chunk size is missing")?)
            .map_err(|_| "a chunk size is too large".to_string())?;
        let start = off + 8;
        let stop = start.checked_add(size).ok_or("a chunk size overflows")?;
        if stop > end {
            return Err(format!(
                "chunk `{}` runs past the end of the file",
                String::from_utf8_lossy(id).trim_end()
            ));
        }
        let payload = &data[start..stop];
        match id {
            b"COMM" => out.comm = Some(parse_comm(payload)?),
            b"SSND" => out.ssnd = parse_ssnd(data, start, stop, payload)?,
            b"MARK" => parse_mark(payload, &mut out.loop_start, &mut out.loop_end)?,
            b"APPL"
                if out.app.is_none()
                    && (payload.starts_with(b"REX ") || payload.starts_with(b"ReCy")) =>
            {
                out.app = Some(payload);
            }
            _ => {}
        }
        off = stop + (size & 1);
    }
    Ok(out)
}

fn parse_comm(payload: &[u8]) -> Result<Comm, String> {
    if payload.len() < 18 {
        return Err("the `COMM` chunk is too short".to_string());
    }
    let channels = usize::from(be16_at(payload, 0).ok_or("the channel count is missing")?);
    let frames = be32_at(payload, 2).ok_or("the frame count is missing")?;
    let bits = be16_at(payload, 6).ok_or("the bit depth is missing")?;
    let rate = extended_rate(payload.get(8..18).ok_or("the sample rate is missing")?)
        .ok_or("the sample rate is not a positive 80-bit number")?;
    Ok(Comm {
        channels,
        frames,
        bits,
        rate,
    })
}

/// The `SSND` chunk's PCM, after its offset and block size.
fn parse_ssnd<'a>(
    data: &'a [u8],
    start: usize,
    stop: usize,
    payload: &[u8],
) -> Result<Option<&'a [u8]>, String> {
    if payload.len() < 8 {
        return Ok(None);
    }
    let offset = usize::try_from(be32_at(payload, 0).ok_or("the SSND offset is missing")?)
        .map_err(|_| "the SSND offset is too large".to_string())?;
    let data_start = start
        .checked_add(8)
        .and_then(|v| v.checked_add(offset))
        .ok_or("the SSND offset overflows")?;
    if data_start > stop {
        return Err("the SSND offset runs past the chunk".to_string());
    }
    Ok(Some(&data[data_start..stop]))
}

/// Reads `Loop start` / `Loop end` from an AIFF `MARK` chunk, ignoring other markers.
fn parse_mark(
    payload: &[u8],
    loop_start: &mut Option<u32>,
    loop_end: &mut Option<u32>,
) -> Result<(), String> {
    let count = be16_at(payload, 0).ok_or("the `MARK` chunk is too short")?;
    let mut pos = 2usize;
    for _ in 0..count {
        if pos + 7 > payload.len() {
            return Ok(());
        }
        let position = be32_at(payload, pos + 2).ok_or("a marker position is missing")?;
        let name_len = usize::from(payload[pos + 6]);
        let name_start = pos + 7;
        let name_end = name_start
            .checked_add(name_len)
            .filter(|end| *end <= payload.len())
            .ok_or("a marker name runs past the chunk")?;
        let name = &payload[name_start..name_end];
        if name == b"Loop start" {
            *loop_start = Some(position);
        } else if name == b"Loop end" {
            *loop_end = Some(position);
        }
        // Each marker record is padded to an even length.
        pos = name_end + ((name_len + 1) & 1);
    }
    Ok(())
}

/// The `REX ` slice table: explicit starts and lengths.
fn rex_slices(app: &[u8]) -> Result<(Vec<RexSlice>, Vec<u32>), String> {
    let binary = app.get(4..).ok_or("the `REX ` APPL chunk is too short")?;
    if binary.len() < REX_TABLE {
        return Err("the `REX ` slice table is too short".to_string());
    }
    if be32_at(binary, 0) != Some(REX_MAGIC) {
        return Err("the `REX ` slice table has the wrong magic".to_string());
    }
    let count = usize::from(be16_at(binary, 0x0a).ok_or("the slice count is missing")?);
    if count == 0 || count > MAX_LEGACY_SLICES {
        return Err(format!("the `REX ` table says {count} slices"));
    }
    let needed = REX_TABLE
        .checked_add(count * 12)
        .ok_or("the `REX ` slice table size overflows")?;
    if binary.len() < needed {
        return Err("the `REX ` slice table is truncated".to_string());
    }
    let mut slices = Vec::new();
    let mut markers = Vec::new();
    for i in 0..count {
        let at = REX_TABLE + i * 12;
        let start = be32_at(binary, at).ok_or("a `REX ` slice start is missing")?;
        let length = be32_at(binary, at + 4).ok_or("a `REX ` slice length is missing")?;
        if length == 0 {
            continue;
        }
        if length <= 1 {
            markers.push(start);
        } else {
            slices.push(RexSlice {
                start,
                length,
                muted: false,
                locked: false,
                // The reference reader shows the first record as the selected slice.
                selected: i == 0,
            });
        }
    }
    Ok((slices, markers))
}

/// The `ReCy` slice table: starts, states and analysis points, with derived lengths.
fn recy_slices(
    app: &[u8],
    frames: u32,
    loop_end: u32,
) -> Result<(Vec<RexSlice>, Vec<u32>), String> {
    let binary = app.get(4..).ok_or("the `ReCy` APPL chunk is too short")?;
    if binary.len() < RECY_TABLE {
        return Err("the `ReCy` slice table is too short".to_string());
    }
    if be32_at(binary, 0) != Some(RECY_MAGIC) {
        return Err("the `ReCy` slice table has the wrong magic".to_string());
    }
    let sensitivity = be16_at(binary, 0x14).ok_or("the sensitivity is missing")?;
    let filter = 0x7fffu32.saturating_sub(visible_range(sensitivity));
    let count = usize::from(be16_at(binary, 0x9e).ok_or("the slice count is missing")?);
    if count == 0 || count > MAX_LEGACY_SLICES {
        return Err(format!("the `ReCy` table says {count} slices"));
    }
    let needed = RECY_TABLE
        .checked_add(count * 8)
        .ok_or("the `ReCy` slice table size overflows")?;
    if binary.len() < needed {
        return Err("the `ReCy` slice table is truncated".to_string());
    }

    struct Entry {
        start: u32,
        state: u8,
        selected: bool,
    }
    let mut entries = Vec::with_capacity(count);
    for i in 0..count {
        let at = RECY_TABLE + i * 8;
        let flags = binary[at];
        let state = flags & 0x7f;
        let selected = flags & 0x80 != 0;
        let start = be32_at(binary, at + 1).ok_or("a `ReCy` slice start is missing")?;
        let points = be16_at(binary, at + 6).ok_or("a `ReCy` analysis point is missing")?;
        // A slice the analysis pass did not select and that does not stand out is dropped,
        // as the reference reader does.
        if !selected && (state != 0 || u32::from(points) <= filter) {
            continue;
        }
        entries.push(Entry {
            start,
            state,
            selected,
        });
    }
    entries.sort_by_key(|e| e.start);
    entries.dedup_by_key(|e| e.start);

    let end = if loop_end > 0 { loop_end } else { frames };
    let mut slices = Vec::with_capacity(entries.len());
    for (i, entry) in entries.iter().enumerate() {
        let next = entries.get(i + 1).map_or(end, |e| e.start);
        let mut length = next.saturating_sub(entry.start).max(1);
        if entry.start.saturating_add(length) > frames {
            length = frames.saturating_sub(entry.start).max(1);
        }
        slices.push(RexSlice {
            start: entry.start,
            length,
            muted: entry.state == 2,
            locked: entry.state == 1,
            selected: entry.selected,
        });
    }
    Ok((slices, Vec::new()))
}

/// How many of the 0x7FFF analysis points the sensitivity setting hides.
fn visible_range(sensitivity: u16) -> u32 {
    let sensitivity = u32::from(sensitivity).min(1000);
    (sensitivity * 0x7fff).div_ceil(1000)
}

/// The preview tempo from the APPL header, in BPM.
fn legacy_tempo(app: &[u8], is_recy: bool) -> Option<f64> {
    let at = if is_recy { 14 } else { 16 };
    let milli = be32_at(app, at)?;
    (20_000..=450_000)
        .contains(&milli)
        .then_some(f64::from(milli) / 1000.0)
}

/// Big-endian AIFF PCM as interleaved `i16` at 16-bit scale.
fn decode_pcm(ssnd: &[u8], channels: usize, bits: u16, frames: u32) -> Vec<i16> {
    let bytes = usize::from(bits / 8);
    let frame_bytes = bytes * channels;
    let mut out = Vec::with_capacity(frames as usize * channels);
    for frame in 0..frames as usize {
        let base = frame * frame_bytes;
        for channel in 0..channels {
            let at = base + channel * bytes;
            let Some(sample) = ssnd.get(at..at + bytes) else {
                out.push(0);
                continue;
            };
            let value = match bits {
                8 => i16::from(sample[0] as i8) << 8,
                16 => i16::from_be_bytes([sample[0], sample[1]]),
                24 => {
                    let v = (i32::from(sample[0]) << 16)
                        | (i32::from(sample[1]) << 8)
                        | i32::from(sample[2]);
                    let v = if v & 0x80_0000 != 0 {
                        v | !0xFF_FFFF
                    } else {
                        v
                    };
                    (v >> 8) as i16
                }
                _ => {
                    let v = i32::from_be_bytes([sample[0], sample[1], sample[2], sample[3]]);
                    (v >> 16) as i16
                }
            };
            out.push(value);
        }
    }
    out
}

/// An 80-bit IEEE 754 extended number, as AIFF stores the sample rate.
fn extended_rate(bytes: &[u8]) -> Option<u32> {
    let expon = u16::from_be_bytes([*bytes.first()?, *bytes.get(1)?]);
    let mut mantissa = 0u64;
    for &b in bytes.get(2..10)? {
        mantissa = (mantissa << 8) | u64::from(b);
    }
    if expon == 0 || mantissa == 0 {
        return None;
    }
    let sign = if expon & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = i32::from(expon & 0x7fff) - 16383;
    let value = sign * (mantissa as f64) * 2f64.powi(exp - 63);
    if !value.is_finite() || value <= 0.0 || value > f64::from(u32::MAX) {
        return None;
    }
    Some(value.round() as u32)
}

fn be16_at(data: &[u8], at: usize) -> Option<u16> {
    let bytes = data.get(at..at.checked_add(2)?)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn be32_at(data: &[u8], at: usize) -> Option<u32> {
    let bytes = data.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}
