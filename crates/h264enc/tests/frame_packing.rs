//! Stereo pictures: the frame packing arrangement SEI message (Rec. ITU-T H.264 (V15) 08/2024 D.1.26) the encoder sends
//! with every access unit, read back by FilmCraft's own SEI parser and, as an outside oracle, by
//! ffprobe (skipped when absent).

mod common;

use common::{annexb_nals, ffprobe, out_dir, synth};
use filmcraft_h264::sei::{self, FramePacking as Parsed};
use filmcraft_h264enc::*;
use std::process::Command;

const W: usize = 64;
const H: usize = 32;

/// An Annex-B stream of `n` frames with B-frames and a short GOP.
fn encode(frame_packing: Option<FramePacking>, n: usize) -> Vec<u8> {
    let mut c = EncoderConfig::new(W as u32, H as u32, 25, 1);
    c.bframes = 2;
    c.keyint = 4;
    c.threads = 1;
    c.frame_packing = frame_packing;
    let mut enc = Encoder::new(c).unwrap();
    let mut out = Vec::new();
    for i in 0..n {
        for p in enc.encode(&synth(W, H, i).frame(), i as i64).unwrap() {
            out.extend(p.data);
        }
    }
    for p in enc.flush() {
        out.extend(p.data);
    }
    out
}

/// The frame packing messages of each access unit (an access unit starts at an AUD).
fn messages_per_access_unit(stream: &[u8]) -> Vec<Vec<Parsed>> {
    let mut units: Vec<Vec<Parsed>> = Vec::new();
    for r in annexb_nals(stream) {
        let nal = &stream[r.start + 3..r.end];
        let nal = nal.strip_suffix(&[0]).unwrap_or(nal);
        match nal.first().map(|b| b & 0x1f) {
            Some(9) => units.push(Vec::new()),
            Some(6) => {
                if let (Some(au), Some(f)) = (units.last_mut(), sei::frame_packing(nal)) {
                    au.push(f);
                }
            }
            _ => {}
        }
    }
    units
}

#[test]
fn every_access_unit_carries_the_message() {
    let stream = encode(Some(FramePacking::SIDE_BY_SIDE_LEFT_FIRST), 10);
    let units = messages_per_access_unit(&stream);
    assert_eq!(units.len(), 10);
    let want = Parsed {
        id: 0,
        cancel: false,
        arrangement_type: 3,
        quincunx_sampling: false,
        content_interpretation_type: 1,
        spatial_flipping: false,
        frame0_flipped: false,
        field_views: false,
        current_frame_is_frame0: false,
        frame0_self_contained: false,
        frame1_self_contained: false,
        grid_positions: Some([0, 0, 0, 0]),
        repetition_period: 1,
    };
    for (i, au) in units.iter().enumerate() {
        assert_eq!(au, &vec![want], "access unit {i}");
    }
    // without the setting, no access unit has one
    assert!(messages_per_access_unit(&encode(None, 4)).iter().all(|au| au.is_empty()));
    // the NAL unit for other streams is the one the encoder writes
    assert_eq!(sei::frame_packing(&FramePacking::SIDE_BY_SIDE_LEFT_FIRST.sei_nal()), Some(want));
    let fp = FramePacking { arrangement_type: 4, content_interpretation_type: 2, grid_position: (8, 8) };
    let m = sei::frame_packing(&fp.sei_nal()).unwrap();
    assert_eq!((m.arrangement_type, m.content_interpretation_type, m.grid_positions), (4, 2, Some([8, 8, 8, 8])));
}

#[test]
fn spatial_arrangements_and_view_orders() {
    for (arrangement_type, content_interpretation_type) in [(0, 1), (1, 2), (2, 1), (3, 2), (4, 1), (4, 2), (6, 0), (7, 1)] {
        let fp = FramePacking { arrangement_type, content_interpretation_type, grid_position: (0, 0) };
        let units = messages_per_access_unit(&encode(Some(fp), 3));
        for au in &units {
            assert_eq!(au.len(), 1);
            let m = au[0];
            assert_eq!((m.arrangement_type, m.content_interpretation_type), (arrangement_type, content_interpretation_type));
            // the checkerboard is quincunx sampled (D.2.26) and has no grid positions
            assert_eq!(m.quincunx_sampling, arrangement_type == 0);
            assert_eq!(m.grid_positions.is_none(), arrangement_type == 0);
        }
    }
}

#[test]
fn temporal_interleaving_and_reserved_values_are_refused() {
    for fp in [
        FramePacking { arrangement_type: 5, content_interpretation_type: 1, grid_position: (0, 0) },
        FramePacking { arrangement_type: 8, content_interpretation_type: 1, grid_position: (0, 0) },
        FramePacking { arrangement_type: 3, content_interpretation_type: 3, grid_position: (0, 0) },
        FramePacking { arrangement_type: 3, content_interpretation_type: 1, grid_position: (16, 0) },
    ] {
        let mut c = EncoderConfig::new(W as u32, H as u32, 25, 1);
        c.frame_packing = Some(fp);
        assert!(Encoder::new(c).is_err(), "{fp:?}");
    }
}

/// ffprobe reads the side-by-side mark on every frame (FFmpeg's `stereo_mode` frame tag and its
/// Stereo 3D side data).
#[test]
fn ffprobe_reads_side_by_side_on_every_frame() {
    let Some(probe) = ffprobe() else { return };
    let path = out_dir("frame_packing").join("sbs_left_first.h264");
    std::fs::write(&path, encode(Some(FramePacking::SIDE_BY_SIDE_LEFT_FIRST), 10)).unwrap();
    let out = Command::new(probe)
        .args(["-v", "error", "-show_frames", "-show_entries", "frame=pict_type:frame_tags=stereo_mode:frame_side_data=side_data_type", "-of", "json"])
        .arg(&path)
        .output()
        .expect("run ffprobe");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let json = String::from_utf8_lossy(&out.stdout);
    assert_eq!(json.matches("\"pict_type\"").count(), 10, "{json}");
    assert_eq!(json.matches("\"stereo_mode\": \"left_right\"").count(), 10, "{json}");
    assert_eq!(json.matches("\"side_data_type\": \"Stereo 3D\"").count(), 10, "{json}");
}
