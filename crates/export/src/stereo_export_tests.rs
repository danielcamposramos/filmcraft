//! Exports of a stereo sequence carry the stereo mark: the frame packing arrangement SEI message
//! in every H.264 access unit (FilmCraft's encoder writes it; streams of other encoders, the
//! platforms' hardware encoders, get it inserted) and in every H.265 one, and the `st3d` box of
//! MP4 / QuickTime sample entries. A 2D sequence's exports carry neither. FilmCraft reads every
//! one back as full side by side, left eye first.

use std::sync::Arc;

use filmcraft_isobmff::{CodecConfig, SampleEntry, TrackKind};
use filmcraft_media::generators::GeneratorSource;
use filmcraft_media::stereo::{Eye, StereoPacking};
use filmcraft_media::{Generator, MediaSource};
use filmcraft_project::{ItemKind, Label, MediaClip, MediaRef, SequenceSettings};
use filmcraft_render::SourceMap;
use filmcraft_time::TICKS_PER_SECOND;

use super::*;

const W: u32 = 128;
const H: u32 = 36;
const FRAMES: i64 = 6;

/// A `W`×`H` 24 fps sequence of a colour matte, `stereo` or not, six frames long.
fn sequence(stereo: bool) -> (Arc<Project>, ItemId, SourceMap) {
    let mut p = Project::new("stereo");
    let r = FrameRate::FPS_24;
    let g = GeneratorSource::new(Generator::ColorMatte { color: [0.2, 0.4, 0.8, 1.0] }, W, H, r, Tick(TICKS_PER_SECOND));
    let info = g.info().clone();
    let id = p.add_item(
        "matte",
        Label::Iris,
        ItemKind::Media(MediaClip {
            media: MediaRef::Generator(g.generator.clone()),
            info,
            interpret: Default::default(),
            mark_in: None,
            mark_out: None,
            markers: vec![],
            offline: false,
            proxy: None,
            identity: None,
        }),
        None,
    );
    let seq = p.new_sequence("s", SequenceSettings { width: W, height: H, frame_rate: r, stereo, ..Default::default() }, 1, 0, None);
    let mut v = p.make_track_item(id, filmcraft_project::TrackKind::Video, Tick::ZERO, TimeRange::new(Tick::ZERO, r.tick_of(FRAMES)), r).unwrap();
    for e in &mut v.effects {
        filmcraft_project::resolve_auto_points(e, (W, H), (W, H));
    }
    p.sequence_mut(seq).unwrap().video_tracks[0].items.push(v);
    let mut m = SourceMap::default();
    m.0.insert(id, Arc::new(g));
    (Arc::new(p), seq, m)
}

fn scratch(name: &str) -> String {
    let d = filmcraft_testkit::workspace_root().join("target").join("export-tests").join(format!("stereo-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join(name).to_string_lossy().to_string()
}

/// The video sample entry and every video sample of an MP4 / MOV file.
fn video_samples(path: &str) -> (SampleEntry, Vec<Vec<u8>>) {
    let bytes = std::fs::read(path).unwrap();
    let f = filmcraft_isobmff::open(&bytes[..]).unwrap();
    let i = f.tracks.iter().position(|t| t.kind == TrackKind::Video).unwrap();
    let samples = (0..f.tracks[i].samples.len()).map(|k| f.read_sample(&bytes[..], i, k).unwrap()).collect();
    (f.tracks[i].entries[0].clone(), samples)
}

/// (frame packing type, content interpretation) of the messages of an H.264 or H.265 sample.
fn frame_packing(sample: &[u8], entry: &SampleEntry) -> Vec<(u8, u8)> {
    let (n, hevc) = match &entry.codec {
        CodecConfig::Avc(c) => (c.length_size as usize, false),
        CodecConfig::Hevc(c) => (c.length_size as usize, true),
        _ => return Vec::new(),
    };
    let nals = filmcraft_bitstream::length_prefixed_nals(sample, n).unwrap();
    if hevc {
        nals.iter().filter_map(|n| filmcraft_hevc::sei::frame_packing(n).0).map(|f| (f.arrangement_type, f.content_interpretation_type)).collect()
    } else {
        nals.iter().filter_map(|n| filmcraft_h264::sei::frame_packing(n)).map(|f| (f.arrangement_type, f.content_interpretation_type)).collect()
    }
}

#[test]
fn h264_and_quicktime_exports_carry_the_mark() {
    for stereo in [true, false] {
        let (p, seq, m) = sequence(stereo);
        for (format, ext) in [(Format::H264, "mp4"), (Format::ProRes, "mov"), (Format::Mjpeg, "mov")] {
            let path = scratch(&format!("{}-{stereo}.{ext}", format.id()));
            let s = ExportSettings { format, path: path.clone(), include_audio: false, ..Default::default() };
            export(&p, seq, &s, &m, &Progress::default()).unwrap();
            let (entry, samples) = video_samples(&path);
            assert_eq!(samples.len(), FRAMES as usize);
            // st3d: left-right, whatever the codec
            assert_eq!(entry.video.as_ref().and_then(|v| v.stereo_mode), stereo.then_some(2), "{format:?} stereo {stereo}");
            if format == Format::H264 {
                for (i, smp) in samples.iter().enumerate() {
                    assert_eq!(frame_packing(smp, &entry), if stereo { vec![(3, 1)] } else { vec![] }, "sample {i}, stereo {stereo}");
                }
            }
            // read back: a stereo pair, full side by side, left eye first, as written
            let bytes: Arc<[u8]> = std::fs::read(&path).unwrap().into();
            let v = filmcraft_codecs::open_bytes(ext, bytes).unwrap().info().video.clone().unwrap();
            match v.stereo {
                Some(l) => {
                    assert!(stereo, "{format:?}: a 2D export read as {l:?}");
                    assert_eq!((l.packing, l.first, l.half), (StereoPacking::SideBySide, Eye::Left, false), "{format:?}");
                }
                None => assert!(!stereo, "{format:?}: the mark was not read back"),
            }
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// A stand-in for a hardware encoder: every frame is one H.264 or H.265 slice NAL unit (no SEI),
/// length-prefixed. Factories take it only for paths naming it, so other tests never get it.
struct PlainEncoder {
    hevc: bool,
    rate: FrameRate,
}

impl VideoEncoder for PlainEncoder {
    fn sample_entry(&self) -> SampleEntry {
        if self.hevc {
            let cfg = filmcraft_isobmff::HevcConfig {
                general_profile_idc: 1,
                chroma_format_idc: 1,
                bit_depth_luma: 8,
                bit_depth_chroma: 8,
                length_size: 4,
                ..Default::default()
            };
            SampleEntry::hevc(cfg, W as u16, H as u16)
        } else {
            SampleEntry::avc(filmcraft_isobmff::AvcConfig::new(vec![vec![0x67, 0x64, 0x00, 0x1F]], vec![vec![0x68, 0xEB]], 4), W as u16, H as u16)
        }
    }
    fn timescale(&self) -> u32 {
        self.rate.num as u32
    }
    fn encode(&mut self, f: &EncoderFrame) -> Result<Vec<EncodedPacket>> {
        // an IDR slice (H.264 type 5, H.265 IDR_W_RADL with first_slice_segment_in_pic_flag)
        let nal: &[u8] = if self.hevc { &[0x26, 0x01, 0xAF, 0x08, 0x42] } else { &[0x65, 0x88, 0x84, 0x21] };
        let mut data = (nal.len() as u32).to_be_bytes().to_vec();
        data.extend_from_slice(nal);
        let _ = f.index;
        Ok(vec![EncodedPacket { data, key: true, duration: self.rate.den as u32, composition_offset: 0 }])
    }
    fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        Ok(Vec::new())
    }
}

fn plain_factory(format: Format, _w: u32, _h: u32, rate: FrameRate, s: &ExportSettings) -> Option<Result<Box<dyn VideoEncoder>>> {
    if !s.path.contains("plain-encoder") || !matches!(format, Format::H264 | Format::Hevc) {
        return None;
    }
    Some(Ok(Box::new(PlainEncoder { hevc: format == Format::Hevc, rate })))
}

#[test]
fn streams_of_other_encoders_get_the_message() {
    register_encoder(plain_factory);
    let (p, seq, m) = sequence(true);
    for (format, want) in [(Format::H264, (3, 1)), (Format::Hevc, (3, 1))] {
        let path = scratch(&format!("plain-encoder-{}.mp4", format.id()));
        let s = ExportSettings { format, path: path.clone(), include_audio: false, ..Default::default() };
        export(&p, seq, &s, &m, &Progress::default()).unwrap();
        let (entry, samples) = video_samples(&path);
        assert_eq!(entry.video.as_ref().and_then(|v| v.stereo_mode), Some(2), "{format:?}");
        assert_eq!(samples.len(), FRAMES as usize);
        for (i, smp) in samples.iter().enumerate() {
            assert_eq!(frame_packing(smp, &entry), vec![want], "{format:?} sample {i}");
            // before the slice, which is unchanged
            let nals = filmcraft_bitstream::length_prefixed_nals(smp, 4).unwrap();
            assert_eq!(nals.len(), 2, "{format:?} sample {i}");
        }
        let _ = std::fs::remove_file(&path);
    }
    // a 2D sequence's streams are left as they are
    let (p, seq, m) = sequence(false);
    let path = scratch("plain-encoder-2d.mp4");
    export(&p, seq, &ExportSettings { format: Format::Hevc, path: path.clone(), include_audio: false, ..Default::default() }, &m, &Progress::default())
        .unwrap();
    let (entry, samples) = video_samples(&path);
    assert_eq!(entry.video.as_ref().and_then(|v| v.stereo_mode), None);
    assert!(samples.iter().all(|s| frame_packing(s, &entry).is_empty() && filmcraft_bitstream::length_prefixed_nals(s, 4).unwrap().len() == 1));
    let _ = std::fs::remove_file(&path);
}
