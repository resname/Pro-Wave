//! Import audio files as new audio tracks (Logic Pro: File > Import > Audio File, or drag-and-drop).
use crate::model::{AudioRegion, Project, Region, RegionParams, Track, TrackKind, TrackParams};
use prowave_engine::{write_wav, AudioError, AudioFile};
use prowave_rex::RexFile;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

const PEAK_BINS: usize = 4096;

pub fn is_audio_path(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "wav"
                | "wave"
                | "aif"
                | "aiff"
                | "aifc"
                | "flac"
                | "mp3"
                | "caf"
                | "ogg"
                | "oga"
                | "m4a"
                | "mp4"
                | "rx2"
                | "rex"
        )
    })
}

/// True for the REX2 formats this module decodes itself.
fn is_rex_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "rx2" | "rex"))
}

fn file_name(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Audio")
        .to_string()
}

/// Decodes a REX2 loop to a WAV in this session's media folder and opens it.
///
/// REX2 stores one compressed waveform and slice points rather than one file per slice;
/// the whole waveform becomes the region until slice-aware playback exists. The WAV is
/// written under the session's media folder, which project save copies into the archive.
fn import_rex(path: &Path) -> Result<(AudioFile, String), AudioError> {
    let bytes = std::fs::read(path)?;
    let rex = RexFile::parse(&bytes).map_err(AudioError::Decode)?;
    let dir = crate::project_io::rex_imports_dir();
    std::fs::create_dir_all(&dir)?;
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let name = file_name(path);
    let wav = dir.join(format!(
        "{}-{}.wav",
        name.chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            })
            .collect::<String>(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let channels = usize::from(rex.channels).clamp(1, 2);
    let mut stereo = Vec::with_capacity(rex.frame_count() * 2);
    for frame in 0..rex.frame_count() {
        let left = rex.sample(frame, 0);
        stereo.push(left);
        stereo.push(if channels == 2 {
            rex.sample(frame, 1)
        } else {
            left
        });
    }
    write_wav(&wav, &stereo, rex.sample_rate)?;
    Ok((AudioFile::open(&wav)?, name))
}

/// Wraps an audio file as a region source and computes its waveform overview on a
/// background thread (the region shows a flat line until it is ready).
pub fn audio_region(file: AudioFile, ctx: &egui::Context) -> AudioRegion {
    let peaks = Arc::new(OnceLock::new());
    {
        let (file, peaks, ctx) = (file.clone(), peaks.clone(), ctx.clone());
        std::thread::spawn(move || match file.peaks(PEAK_BINS) {
            Ok(v) => {
                let _ = peaks.set(v);
                ctx.request_repaint();
            }
            Err(e) => tracing::warn!("waveform overview failed for {}: {e}", file.path.display()),
        });
    }
    AudioRegion {
        file,
        peaks,
        flex: None,
    }
}

/// Adds a new audio track with one region starting at the bar under the playhead.
///
/// `.rx2`/`.rex` files are decoded by [`import_rex`] first; every other format is opened by
/// the engine's audio reader.
pub fn import_audio(p: &mut Project, path: &Path, ctx: &egui::Context) -> Result<(), AudioError> {
    let (file, name) = if is_rex_path(path) {
        import_rex(path)?
    } else {
        (AudioFile::open(path)?, file_name(path))
    };
    add_audio_track(p, file, name, ctx)
}

/// Adds `file` as a new audio track with one region starting at the bar under the playhead.
fn add_audio_track(
    p: &mut Project,
    file: AudioFile,
    name: String,
    ctx: &egui::Context,
) -> Result<(), AudioError> {
    let start_bar = p.beats_to_bar(p.position_beats).floor() + 1.0;
    // The region's length follows the Tempo track from where it starts.
    let map = p.tempo_map();
    let start = p.bar_to_beats(start_bar - 1.0);
    let end = map.advance(start, file.duration_secs());
    let len_bars = p.beats_to_bar(end) - (start_bar - 1.0);

    p.tracks.push(Track {
        name: name.clone(),
        kind: TrackKind::Audio,
        color: egui::Color32::from_rgb(0x3f, 0x5f, 0xc0),
        mute: false,
        solo: false,
        rec: false,
        monitor: false,
        input: prowave_engine::InputSource::Mono(0),
        output: 0,
        volume_db: 0.0,
        pan: 0.0,
        inserts: vec![],
        instrument: None,
        regions: vec![Region {
            name,
            start_bar: start_bar as f32,
            len_bars: len_bars as f32,
            notes: vec![],
            controls: Vec::new(),
            folder: None,
            generated: None,
            audio: Some(audio_region(file, ctx)),
            params: RegionParams::default(),
        }],
        params: TrackParams::default(),
        category: None,
        sends: Vec::new(),
        auto: crate::model::Automation::default(),
        group: None,
        height: 1.0,
        indent: 0,
        folded: false,
        vca: None,
        midi_port: None,
        soundfont: None,
        icon: None,
        midi_channel: 1,
        midi_program: None,
        midi_bank: None,
        midi_fx: Vec::new(),
    });
    p.selected_track = p.tracks.len() - 1;
    p.song_rev += 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use prowave_rex::write::{self as rex_write, RexData};

    #[test]
    fn rex_paths_are_audio_files() {
        assert!(is_audio_path(Path::new("loop.rx2")));
        assert!(is_audio_path(Path::new("loop.REX")));
        assert!(is_rex_path(Path::new("loop.rex")));
        assert!(!is_rex_path(Path::new("loop.wav")));
    }

    #[test]
    fn a_rex_loop_becomes_an_audio_track() {
        let samples: Vec<i16> = (0..4_000)
            .map(|i| ((i as f32 * 0.05).sin() * 12_000.0) as i16)
            .collect();
        let bytes = rex_write::write(&RexData::new(44_100, 1, 120.0, samples)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("break.rx2");
        std::fs::write(&path, bytes).unwrap();

        let mut p = Project::empty();
        let before = p.tracks.len();
        import_audio(&mut p, &path, &egui::Context::default()).unwrap();

        assert_eq!(p.tracks.len(), before + 1);
        let track = &p.tracks[p.selected_track];
        assert_eq!(track.name, "break");
        assert_eq!(track.kind, TrackKind::Audio);
        let region = &track.regions[0];
        assert_eq!(region.name, "break");
        let audio = region.audio.as_ref().expect("an audio source");
        assert_eq!(audio.file.sample_rate, 44_100);
        assert_eq!(audio.file.frames, 4_000);
        // The mono loop is upmixed to the stereo WAV the engine opens.
        assert_eq!(audio.file.channels, 2);
        assert!((audio.file.duration_secs() - 4_000.0 / 44_100.0).abs() < 1e-9);
    }
}
