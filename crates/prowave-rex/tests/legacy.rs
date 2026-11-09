//! Legacy AIFF-based ReCycle files: a synthetic `.rex` and `.rcy` are built here and read
//! back, and damaged containers must be refused without a panic.

use prowave_rex::{RexFile, RexSlice};

fn chunk(id: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(id);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        out.push(0);
    }
    out
}

/// An 80-bit IEEE 754 extended number, the way AIFF stores a sample rate.
fn extended_rate(rate: u32) -> [u8; 10] {
    let exponent = 31 - rate.leading_zeros();
    let mantissa = u64::from(rate) << (63 - exponent);
    let mut out = [0u8; 10];
    out[0..2].copy_from_slice(&(16383 + exponent as u16).to_be_bytes());
    out[2..10].copy_from_slice(&mantissa.to_be_bytes());
    out
}

fn comm(channels: u16, frames: u32, bits: u16, rate: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&channels.to_be_bytes());
    payload.extend_from_slice(&frames.to_be_bytes());
    payload.extend_from_slice(&bits.to_be_bytes());
    payload.extend_from_slice(&extended_rate(rate));
    chunk(b"COMM", &payload)
}

fn ssnd(samples: &[i16]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u32.to_be_bytes()); // offset
    payload.extend_from_slice(&0u32.to_be_bytes()); // block size
    for sample in samples {
        payload.extend_from_slice(&sample.to_be_bytes());
    }
    chunk(b"SSND", &payload)
}

fn mark(markers: &[(&str, u32)]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(markers.len() as u16).to_be_bytes());
    for (i, (name, position)) in markers.iter().enumerate() {
        payload.extend_from_slice(&(i as u16 + 1).to_be_bytes());
        payload.extend_from_slice(&position.to_be_bytes());
        payload.push(name.len() as u8);
        payload.extend_from_slice(name.as_bytes());
        // Each marker record is padded to an even length.
        if name.len() % 2 == 0 {
            payload.push(0);
        }
    }
    chunk(b"MARK", &payload)
}

/// A `REX ` APPL chunk: explicit slice starts and lengths at 0x3F8.
fn rex_app(tempo: u32, slices: &[(u32, u32, u32)]) -> Vec<u8> {
    let mut binary = vec![0u8; 0x3f8 + slices.len() * 12];
    binary[0..4].copy_from_slice(&0xd1d1_d1dau32.to_be_bytes());
    binary[6..10].copy_from_slice(&3840u32.to_be_bytes()); // one bar of PPQ
    binary[0x0a..0x0c].copy_from_slice(&(slices.len() as u16).to_be_bytes());
    for (i, (start, length, ppq)) in slices.iter().enumerate() {
        let at = 0x3f8 + i * 12;
        binary[at..at + 4].copy_from_slice(&start.to_be_bytes());
        binary[at + 4..at + 8].copy_from_slice(&length.to_be_bytes());
        binary[at + 8..at + 12].copy_from_slice(&ppq.to_be_bytes());
    }
    let mut payload = b"REX ".to_vec();
    payload.extend_from_slice(&binary);
    payload[16..20].copy_from_slice(&tempo.to_be_bytes());
    chunk(b"APPL", &payload)
}

/// A `ReCy` APPL chunk: flags and starts at 0xA0; lengths run to the next start.
fn recy_app(tempo: u32, slices: &[(u8, u32)]) -> Vec<u8> {
    let mut binary = vec![0u8; 0xa0 + slices.len() * 8];
    binary[0..4].copy_from_slice(&0xd1da_ded0u32.to_be_bytes());
    binary[0x14..0x16].copy_from_slice(&1000u16.to_be_bytes()); // full sensitivity
    binary[0x9e..0xa0].copy_from_slice(&(slices.len() as u16).to_be_bytes());
    for (i, (flags, start)) in slices.iter().enumerate() {
        let at = 0xa0 + i * 8;
        binary[at] = *flags;
        binary[at + 1..at + 5].copy_from_slice(&start.to_be_bytes());
        binary[at + 6..at + 8].copy_from_slice(&0x7fffu16.to_be_bytes());
    }
    let mut payload = b"ReCy".to_vec();
    payload.extend_from_slice(&binary);
    payload[14..18].copy_from_slice(&tempo.to_be_bytes());
    chunk(b"APPL", &payload)
}

fn aiff(children: &[u8]) -> Vec<u8> {
    let mut payload = b"AIFF".to_vec();
    payload.extend_from_slice(children);
    chunk(b"FORM", &payload)
}

fn ramp(frames: usize) -> Vec<i16> {
    (0..frames)
        .map(|i| (i as i16).wrapping_mul(13).wrapping_sub(6000))
        .collect()
}

#[test]
fn reads_a_legacy_rex_file() {
    let samples = ramp(1_000);
    let mut children = Vec::new();
    children.extend_from_slice(&comm(1, 1_000, 16, 44_100));
    children.extend_from_slice(&mark(&[("Loop start", 10), ("Loop end", 990)]));
    children.extend_from_slice(&ssnd(&samples));
    children.extend_from_slice(&rex_app(120_000, &[(0, 500, 0), (500, 500, 1920)]));
    let bytes = aiff(&children);

    let rex = RexFile::parse(&bytes).expect("the legacy loop should parse");
    assert_eq!(rex.channels, 1);
    assert_eq!(rex.sample_rate, 44_100);
    assert_eq!(rex.bit_depth, 16);
    assert_eq!(rex.frames, 1_000);
    assert_eq!(rex.tempo, Some(120.0));
    assert_eq!(rex.original_tempo, Some(120.0));
    assert_eq!(rex.loop_start, 10);
    assert_eq!(rex.loop_end, 990);
    assert_eq!(rex.slices.len(), 2);
    assert_eq!(
        rex.slices[0],
        RexSlice {
            start: 0,
            length: 500,
            muted: false,
            locked: false,
            selected: true,
        }
    );
    assert_eq!(rex.slices[1].start, 500);
    assert_eq!(rex.slices[1].length, 500);
    for (frame, expected) in samples.iter().enumerate() {
        assert_eq!(rex.sample(frame, 0), f32::from(*expected) / 32768.0);
    }
}

#[test]
fn reads_a_legacy_recy_file() {
    let samples = ramp(800);
    let mut children = Vec::new();
    children.extend_from_slice(&comm(1, 800, 16, 48_000));
    children.extend_from_slice(&ssnd(&samples));
    children.extend_from_slice(&recy_app(
        90_000,
        &[
            (0x80 | 0x01, 0),   // selected and locked
            (0x00, 400),        // normal, unselected
            (0x80 | 0x02, 700), // selected and muted
        ],
    ));
    let bytes = aiff(&children);

    let rex = RexFile::parse(&bytes).expect("the legacy ReCycle file should parse");
    assert_eq!(rex.sample_rate, 48_000);
    assert_eq!(rex.tempo, Some(90.0));
    assert_eq!(rex.frames, 800);
    assert_eq!(rex.loop_start, 0);
    assert_eq!(rex.loop_end, 800);
    assert_eq!(rex.slices.len(), 3);
    assert_eq!(rex.slices[0].start, 0);
    assert_eq!(rex.slices[0].length, 400);
    assert!(rex.slices[0].selected);
    assert!(rex.slices[0].locked);
    assert_eq!(rex.slices[1].start, 400);
    assert_eq!(rex.slices[1].length, 300);
    assert!(!rex.slices[1].muted && !rex.slices[1].locked);
    assert_eq!(rex.slices[2].start, 700);
    assert_eq!(rex.slices[2].length, 100);
    assert!(rex.slices[2].muted);
}

#[test]
fn a_24_bit_legacy_file_is_reduced_to_16_bit() {
    // 0x100000 is 1.0 in 24-bit scale; the crate returns the top 16 bits.
    let values: [i32; 4] = [0x10_0000, -0x10_0000, 0x7f_ffff, -0x80_0000];
    let mut pcm = Vec::new();
    for value in values {
        pcm.extend_from_slice(&value.to_be_bytes()[1..4]);
    }
    let mut ssnd_payload = Vec::new();
    ssnd_payload.extend_from_slice(&0u32.to_be_bytes());
    ssnd_payload.extend_from_slice(&0u32.to_be_bytes());
    ssnd_payload.extend_from_slice(&pcm);
    let mut children = Vec::new();
    children.extend_from_slice(&comm(1, 4, 24, 44_100));
    children.extend_from_slice(&chunk(b"SSND", &ssnd_payload));
    children.extend_from_slice(&rex_app(120_000, &[(0, 4, 0)]));
    let bytes = aiff(&children);

    let rex = RexFile::parse(&bytes).expect("the 24-bit legacy loop should parse");
    assert_eq!(rex.bit_depth, 24);
    assert!(rex.warnings.iter().any(|w| w.contains("reduced to 16-bit")));
    let expected: Vec<i16> = values.iter().map(|v| (v >> 8) as i16).collect();
    for (frame, value) in expected.iter().enumerate() {
        assert_eq!(rex.sample(frame, 0), f32::from(*value) / 32768.0);
    }
}

#[test]
fn damaged_legacy_files_are_refused_without_panicking() {
    let samples = ramp(600);
    let mut children = Vec::new();
    children.extend_from_slice(&comm(1, 600, 16, 44_100));
    children.extend_from_slice(&ssnd(&samples));
    children.extend_from_slice(&rex_app(120_000, &[(0, 300, 0), (300, 300, 1920)]));
    let bytes = aiff(&children);

    for cut in (0..bytes.len()).step_by(37) {
        let _ = RexFile::parse(&bytes[..cut]);
    }
    // An ordinary AIFF is not a ReCycle file.
    let mut plain = Vec::new();
    plain.extend_from_slice(&comm(1, 100, 16, 44_100));
    plain.extend_from_slice(&ssnd(&[0; 100]));
    assert!(RexFile::parse(&aiff(&plain)).is_err());
    // Random junk that claims to be an AIFF.
    let mut seed = 0x9e37_79b9u32;
    for _ in 0..2_000 {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let mut junk: Vec<u8> = (0..((seed as usize >> 12) % 400 + 12))
            .map(|i| (i as u32).wrapping_mul(seed) as u8)
            .collect();
        junk[0..4].copy_from_slice(b"FORM");
        junk[8..12].copy_from_slice(b"AIFF");
        let form_size = junk.len() as u32 - 8;
        junk[4..8].copy_from_slice(&form_size.to_be_bytes());
        let _ = RexFile::parse(&junk);
    }
}
