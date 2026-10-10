//! Stereo 3D media through the session: a file marked as a half-width side-by-side pair imports
//! as full side by side (left eye first); a sequence made from it is stereo and shows that picture;
//! VR effects applied to it start with the side-by-side frame layout; Interpret Footage reads the
//! frames another way; exports carry the mark (a frame packing SEI message in every access unit,
//! an `st3d` box) and import again as the same pair. The test media come from FilmCraft's own
//! encoder; ffprobe, when present, is an outside oracle for the exported mark.

use std::path::{Path, PathBuf};
use std::process::Command;

use filmcraft_media::stereo::{Eye, StereoMark, StereoPacking};
use filmcraft_project::{ItemId, ParamValue};
use serde_json::{Value, json};

use crate::Session;
use crate::media_test_util::{make_pair, tmp_dir};

const DARK: u8 = 60;
const BRIGHT: u8 = 200;

/// An H.264 SEI NAL unit written out bit by bit from the syntax table (Rec. ITU-T H.264 D.1.26),
/// not by FilmCraft: a frame packing arrangement message, side by side (type 3), constituent frame
/// 0 is the left view (content interpretation 1), default grid positions, repetition period 1.
const SBS_LEFT_FIRST_SEI: [u8; 12] = [0x06, 0x2D, 0x07, 0x81, 0x81, 0x00, 0x00, 0x03, 0x00, 0x01, 0x20, 0x80];

/// A 64×32 pair, the left eye dark and the right eye bright, marked side by side: two 32×32
/// halves, read as half-width views.
fn marked_pair(dir: &Path) -> PathBuf {
    let p = dir.join("pair.mp4");
    make_pair(&p, 64, 32, DARK, BRIGHT, Some(&SBS_LEFT_FIRST_SEI), 6);
    p
}

fn import(s: &mut Session, p: &Path) -> ItemId {
    let r = s.execute("file.import", json!({"paths": [p.to_string_lossy()]})).unwrap();
    ItemId(r["items"][0].as_u64().unwrap())
}

fn video(s: &Session, item: ItemId) -> filmcraft_media::VideoStreamInfo {
    s.project.item(item).unwrap().as_media().unwrap().info.video.clone().unwrap()
}

fn frame_layout(s: &Session, clip: filmcraft_project::ClipId, effect: &str) -> Option<ParamValue> {
    let q = s.active_sequence().unwrap();
    let (_, it) = q.find_item(clip).unwrap();
    it.effects.iter().find(|e| e.effect == effect).and_then(|e| e.param("frame_layout")).map(|p| p.value.clone())
}

/// Mean luma-ish level (mean of R, G, B in 0..255 sRGB) of columns `x0..x1` of an image.
fn level(img: &filmcraft_render::Image, x0: usize, x1: usize) -> f64 {
    let rgba = img.over_black_rgba8();
    let mut sum = 0.0;
    let mut n = 0.0;
    for y in 0..img.h {
        for x in x0..x1 {
            let p = &rgba[(y * img.w + x) * 4..(y * img.w + x) * 4 + 3];
            sum += p.iter().map(|&v| v as f64).sum::<f64>() / 3.0;
            n += 1.0;
        }
    }
    sum / n
}

/// The Source monitor's picture of an item (its first frame).
fn source_picture(s: &Session, item: ItemId) -> filmcraft_render::Image {
    let provider = s.media.provider(s.project.clone(), s.services.clone());
    filmcraft_render::render_item(&s.project, item, filmcraft_time::Tick::ZERO, 1.0, &provider).unwrap()
}

/// Dark on the left half, bright on the right half (each eye's picture).
fn assert_dark_then_bright(img: &filmcraft_render::Image, what: &str) {
    let (l, r) = (level(img, 0, img.w / 2), level(img, img.w / 2, img.w));
    assert!(l < 90.0 && r > 170.0, "{what}: left {l:.0}, right {r:.0}");
}

/// Before stereo support FilmCraft read this clip as a 64×32 picture holding both eyes, and VR
/// effects started Monoscopic on it (the step-1 measurements). Only commands and fields that
/// existed then are used, so this test also runs, and fails, on that code.
#[test]
fn a_marked_pair_is_read_as_a_stereo_pair() {
    let dir = tmp_dir("stereo-regression");
    let mut s = Session::default();
    let item = import(&mut s, &marked_pair(&dir));
    // each half-width eye at its full width, side by side
    let v = video(&s, item);
    assert_eq!((v.width, v.height), (128, 32), "the pair, full side by side");
    s.execute("file.newSequenceFromClip", json!({"items": [item.0]})).unwrap();
    let q = s.active_sequence().unwrap();
    assert_eq!((q.settings.width, q.settings.height), (128, 32));
    let clip = q.video_tracks[0].items[0].id;
    // the VR effects' Frame Layout comes from the clip, not the manual default
    s.execute("effects.apply", json!({"effect": "vr_rotate_sphere", "clips": [clip.0]})).unwrap();
    assert_eq!(frame_layout(&s, clip, "vr_rotate_sphere"), Some(ParamValue::Choice(2)), "Stereoscopic - Side by Side");
    // the Program monitor: the left (dark) eye on the left
    assert_dark_then_bright(&s.render_program(1.0).unwrap(), "Program monitor");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_marked_pair_imports_full_side_by_side() {
    let dir = tmp_dir("stereo-import");
    let mut s = Session::default();
    let item = import(&mut s, &marked_pair(&dir));
    let v = video(&s, item);
    // a 64×32 frame holding two 32×32 halves, read as half-width views: 128×32
    assert_eq!((v.width, v.height), (128, 32));
    let l = v.stereo.expect("stereo layout");
    assert_eq!((l.packing, l.first, l.half, l.mark), (StereoPacking::SideBySide, Eye::Left, true, StereoMark::FramePackingSei));
    // each eye fills its half: the left (dark) eye on the left
    let img = source_picture(&s, item);
    assert_eq!((img.w, img.h), (128, 32));
    assert_dark_then_bright(&img, "Source monitor");
    // the clip's properties say so
    let info = s.execute("media.stereoInfo", json!({"item": item.0})).unwrap();
    assert_eq!(info["file"]["label"], "Side by side, half width, left eye first (frame packing SEI)");
    assert_eq!(info["override"], Value::Null);
    assert_eq!(info["choices"].as_array().unwrap().len(), 19);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_stereo_clip_makes_a_stereo_sequence_and_vr_effects_start_side_by_side() {
    let dir = tmp_dir("stereo-sequence");
    let mut s = Session::default();
    let stereo = import(&mut s, &marked_pair(&dir));
    let plain = dir.join("plain.mp4");
    make_pair(&plain, 128, 32, DARK, BRIGHT, None, 6);
    let mono = import(&mut s, &plain);
    s.execute("file.newSequenceFromClip", json!({"items": [stereo.0]})).unwrap();
    let stereo_seq = s.state.active_sequence.unwrap();
    let q = s.active_sequence().unwrap().clone();
    assert!(q.settings.stereo);
    assert_eq!((q.settings.width, q.settings.height), (128, 32));
    // the Program monitor shows the pair full side by side
    let img = s.render_program(1.0).unwrap();
    assert_eq!((img.w, img.h), (128, 32));
    assert_dark_then_bright(&img, "Program monitor");
    // a sequence from 2D media is not stereo
    s.execute("file.newSequenceFromClip", json!({"items": [mono.0]})).unwrap();
    assert!(!s.active_sequence().unwrap().settings.stereo);
    // VR Blur on the stereo clip starts side by side, on the 2D clip monoscopic
    let mono_clip = s.active_sequence().unwrap().video_tracks[0].items[0].id;
    s.execute("effects.apply", json!({"effect": "vr_blur", "clips": [mono_clip.0]})).unwrap();
    assert_eq!(frame_layout(&s, mono_clip, "vr_blur"), Some(ParamValue::Choice(0)));
    s.state.active_sequence = Some(stereo_seq);
    let stereo_clip = q.video_tracks[0].items[0].id;
    s.execute("effects.apply", json!({"effect": "vr_blur", "clips": [stereo_clip.0]})).unwrap();
    assert_eq!(frame_layout(&s, stereo_clip, "vr_blur"), Some(ParamValue::Choice(2)), "Stereoscopic - Side by Side");
    // Reset Effect gives the clip's layout again, not the manual default
    s.execute("effects.setParam", json!({"clip": stereo_clip.0, "effect": 0, "param": "frame_layout", "value": 1})).unwrap();
    assert_eq!(frame_layout(&s, stereo_clip, "vr_blur"), Some(ParamValue::Choice(1)));
    s.execute("effects.reset", json!({"clip": stereo_clip.0, "index": 0})).unwrap();
    assert_eq!(frame_layout(&s, stereo_clip, "vr_blur"), Some(ParamValue::Choice(2)));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn interpret_footage_reads_the_frames_another_way() {
    let dir = tmp_dir("stereo-interpret");
    let mut s = Session::default();
    let item = import(&mut s, &marked_pair(&dir));
    // 2D: the frames as stored, whatever the mark says
    let r = s.execute("clip.interpretFootage", json!({"items": [item.0], "stereo": "2d"})).unwrap();
    assert_eq!(r["stereo"], "2d");
    let v = video(&s, item);
    assert_eq!((v.width, v.height, v.stereo.map(|l| l.packing)), (64, 32, Some(StereoPacking::Mono)));
    assert!(s.project.item(item).unwrap().as_media().unwrap().stereo().is_none());
    let img = source_picture(&s, item);
    assert_eq!(img.w, 64);
    assert_dark_then_bright(&img, "2D");
    // full-width views, right eye first: the bright half is now the left eye
    s.execute("clip.interpretFootage", json!({"items": [item.0], "stereo": "sbs-right"})).unwrap();
    let v = video(&s, item);
    assert_eq!((v.width, v.height), (64, 32));
    let img = source_picture(&s, item);
    let (l, r) = (level(&img, 0, 32), level(&img, 32, 64));
    assert!(l > 170.0 && r < 90.0, "right eye first: left {l:.0}, right {r:.0}");
    // as marked again, and the mark's own reading comes back
    s.execute("clip.interpretFootage", json!({"items": [item.0], "stereo": "auto"})).unwrap();
    assert_eq!(video(&s, item).width, 128);
    assert_eq!(s.execute("media.stereoInfo", json!({"item": item.0})).unwrap()["override"], Value::Null);
    // one undo step per change
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(video(&s, item).width, 64);
    assert!(s.execute("clip.interpretFootage", json!({"items": [item.0], "stereo": "sideways"})).is_err());
    let _ = std::fs::remove_dir_all(dir);
}

/// (frames, frames whose `stereo_mode` tag is `left_right`) by ffprobe (None without ffprobe).
fn ffprobe_left_right_frames(p: &str) -> Option<(usize, usize)> {
    let probe = filmcraft_testkit::ffprobe_or_skip("stereo export oracle")?;
    let out = Command::new(probe)
        .args(["-v", "error", "-show_frames", "-show_entries", "frame=pict_type:frame_tags=stereo_mode", "-of", "json"])
        .arg(p)
        .output()
        .ok()?;
    let json = String::from_utf8_lossy(&out.stdout).into_owned();
    Some((json.matches("\"pict_type\"").count(), json.matches("\"stereo_mode\": \"left_right\"").count()))
}

#[test]
fn stereo_exports_carry_the_mark() {
    let dir = tmp_dir("stereo-export");
    let mut s = Session::default();
    let item = import(&mut s, &marked_pair(&dir));
    s.execute("file.newSequenceFromClip", json!({"items": [item.0]})).unwrap();
    for (format, ext) in [("h264", "mp4"), ("prores", "mov")] {
        let path = dir.join(format!("out.{ext}")).to_string_lossy().to_string();
        let r = s.execute("file.exportMedia", json!({"path": path, "format": format, "audio": false, "wait": true})).unwrap();
        let out = r["path"].as_str().unwrap().to_string();
        let bytes = std::fs::read(&out).unwrap();
        // the sample entry's st3d box: a FullBox of version 0, stereo_mode 2 (left-right)
        let at = bytes.windows(4).position(|w| w == b"st3d").unwrap_or_else(|| panic!("{format}: no st3d box"));
        assert_eq!(&bytes[at - 4..at + 9], &[0, 0, 0, 13, b's', b't', b'3', b'd', 0, 0, 0, 0, 2]);
        if format == "h264" {
            // every access unit holds the frame packing message
            let f = filmcraft_isobmff::open(&bytes[..]).unwrap();
            let t = f.tracks.iter().find(|t| t.kind == filmcraft_isobmff::TrackKind::Video).unwrap();
            assert_eq!(t.samples.len(), 6);
            for (i, smp) in t.samples.iter().enumerate() {
                let data = &bytes[smp.offset as usize..smp.offset as usize + smp.size as usize];
                let nals = filmcraft_codecs::stereo::length_prefixed(data, 4);
                let fpa: Vec<_> = nals.iter().filter_map(|n| filmcraft_h264::sei::frame_packing(n)).collect();
                assert_eq!(fpa.len(), 1, "sample {i}");
                assert_eq!((fpa[0].arrangement_type, fpa[0].content_interpretation_type), (3, 1), "sample {i}");
            }
            // an outside decoder reads it on every frame
            if let Some(counts) = ffprobe_left_right_frames(&out) {
                assert_eq!(counts, (6, 6));
            }
        }
        // imported again, the export is the same pair, full side by side as written
        let back = import(&mut s, Path::new(&out));
        let v = video(&s, back);
        let l = v.stereo.expect("stereo layout");
        assert_eq!((v.width, v.height, l.packing, l.first, l.half), (128, 32, StereoPacking::SideBySide, Eye::Left, false), "{format}");
        assert_eq!(l.mark, if format == "h264" { StereoMark::FramePackingSei } else { StereoMark::St3d });
        assert_dark_then_bright(&source_picture(&s, back), format);
    }
    let _ = std::fs::remove_dir_all(dir);
}
