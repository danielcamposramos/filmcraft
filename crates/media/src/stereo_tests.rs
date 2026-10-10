use std::sync::Arc;

use filmcraft_color::ColorInfo;
use filmcraft_frame::{AudioBuffer, Chroma, PixelData, VideoFrame};
use filmcraft_time::{FrameRate, Tick};

use super::*;
use crate::{MediaError, MediaKind, MediaSource, VideoStreamInfo};

use Eye::{Left, Right};
use StereoPacking::*;

// ---- marks

#[test]
fn matroska_stereo_modes() {
    // RFC 9559 Table 5
    let table = [
        (0, Mono, Left),
        (1, SideBySide, Left),
        (2, TopBottom, Right),
        (3, TopBottom, Left),
        (4, Checkerboard, Right),
        (5, Checkerboard, Left),
        (6, RowInterleaved, Right),
        (7, RowInterleaved, Left),
        (8, ColumnInterleaved, Right),
        (9, ColumnInterleaved, Left),
        (10, AnaglyphCyanRed, Left),
        (11, SideBySide, Right),
        (12, AnaglyphGreenMagenta, Left),
        (13, FrameSequential, Left),
        (14, FrameSequential, Right),
    ];
    for (mode, packing, first) in table {
        let l = StereoLayout::from_matroska(mode).unwrap();
        assert_eq!((l.packing, l.first, l.half, l.mark), (packing, first, false, StereoMark::Matroska), "StereoMode {mode}");
        assert_eq!(l.to_matroska(), Some(mode));
        assert_eq!(l.is_stereo(), !matches!(packing, Mono | AnaglyphCyanRed | AnaglyphGreenMagenta), "StereoMode {mode}");
    }
    assert_eq!(StereoLayout::from_matroska(15), None);
}

#[test]
fn st3d_stereo_modes() {
    let table = [(0, Mono, Left), (1, TopBottom, Left), (2, SideBySide, Left), (3, Custom, Left), (4, SideBySide, Right)];
    for (mode, packing, first) in table {
        let l = StereoLayout::from_st3d(mode).unwrap();
        assert_eq!((l.packing, l.first, l.half, l.mark), (packing, first, false, StereoMark::St3d), "st3d {mode}");
        assert_eq!(l.to_st3d(), Some(mode));
    }
    assert_eq!(StereoLayout::from_st3d(5), None);
    assert!(!StereoLayout::from_st3d(3).unwrap().is_stereo(), "a custom layout can't be split");
    // st3d can't say top-bottom with the right eye on top, or interleaving
    assert_eq!(StereoLayout::new(TopBottom, Right, StereoMark::Matroska).to_st3d(), None);
    assert_eq!(StereoLayout::new(RowInterleaved, Left, StereoMark::Matroska).to_st3d(), None);
}

#[test]
fn frame_packing_arrangements_and_view_orders() {
    // H.264 Table D-9, both orders of Table D-10 (0, unspecified, reads as frame 0 = left)
    let types = [Checkerboard, ColumnInterleaved, RowInterleaved, SideBySide, TopBottom, FrameSequential, Mono, TileFormat];
    for (t, packing) in types.into_iter().enumerate() {
        for (content, first) in [(0, Left), (1, Left), (2, Right)] {
            let l = StereoLayout::from_frame_packing(t as u8, content, t == 0, false, false, true).unwrap();
            assert_eq!((l.packing, l.mark), (packing, StereoMark::FramePackingSei), "type {t}");
            if packing != Mono {
                assert_eq!(l.first, first, "type {t} content {content}");
                assert_eq!(l.to_frame_packing(), Some((t as u8, if first == Left { 1 } else { 2 })));
            }
        }
    }
    // reserved values are ignored
    assert_eq!(StereoLayout::from_frame_packing(8, 1, false, false, false, false), None);
    assert_eq!(StereoLayout::from_frame_packing(3, 3, false, false, false, false), None);
    // frame alternation: a stream that starts with constituent frame 1 starts with the other view
    assert_eq!(StereoLayout::from_frame_packing(5, 1, false, false, false, false).unwrap().first, Right);
    assert_eq!(StereoLayout::from_frame_packing(5, 2, false, false, false, false).unwrap().first, Left);
    // quincunx side by side is half resolution; spatial flipping names the mirrored view
    let q = StereoLayout::from_frame_packing(3, 1, true, false, false, false).unwrap();
    assert!(q.half && q.quincunx);
    let f = StereoLayout::from_frame_packing(4, 2, false, true, false, false).unwrap();
    assert_eq!((f.first, f.flipped), (Right, Some(Left)), "frame 1 (the left view) is mirrored");
    let f = StereoLayout::from_frame_packing(3, 1, false, true, true, false).unwrap();
    assert_eq!(f.flipped, Some(Left));
}

#[test]
fn sizes() {
    let m = StereoMark::FramePackingSei;
    let half = |p| StereoLayout { half: true, ..StereoLayout::new(p, Left, m) };
    assert_eq!(StereoLayout::new(SideBySide, Left, m).delivered_size(3840, 1080), (3840, 1080));
    assert_eq!(half(SideBySide).eye_size(1920, 1080), (1920, 1080));
    assert_eq!(half(SideBySide).delivered_size(1920, 1080), (3840, 1080));
    assert_eq!(StereoLayout::new(TopBottom, Left, m).delivered_size(1920, 2160), (3840, 1080));
    assert_eq!(half(TopBottom).delivered_size(1920, 1080), (3840, 1080));
    for p in [RowInterleaved, ColumnInterleaved, Checkerboard, FrameSequential] {
        assert_eq!(StereoLayout::new(p, Left, m).delivered_size(1920, 1080), (3840, 1080), "{p:?}");
    }
    // a 1280×720 pair in a 1920×1080 frame
    assert_eq!(StereoLayout::new(TileFormat, Left, m).delivered_size(1920, 1080), (2560, 720));
}

#[test]
fn half_resolution_by_shape() {
    assert!(half_by_shape(SideBySide, 1920, 1080, (1, 1)), "two squeezed 16:9 views");
    assert!(!half_by_shape(SideBySide, 3840, 1080, (1, 1)), "two full 16:9 views");
    assert!(!half_by_shape(SideBySide, 1280, 480, (1, 1)), "two full 4:3 views");
    assert!(half_by_shape(SideBySide, 1920, 800, (1, 1)), "two squeezed 2.39:1 views");
    assert!(half_by_shape(SideBySide, 1440, 1080, (4, 3)), "anamorphic 16:9");
    assert!(half_by_shape(TopBottom, 1920, 1080, (1, 1)));
    assert!(!half_by_shape(TopBottom, 1920, 2160, (1, 1)));
    assert!(!half_by_shape(TopBottom, 3840, 3840, (1, 1)), "two 2:1 panoramas");
    assert!(!half_by_shape(RowInterleaved, 1920, 1080, (1, 1)));
    assert!(!half_by_shape(SideBySide, 0, 1080, (1, 1)));
}

// ---- pixels

/// The views of the tests: linear ramps, so the mean of a view's neighbours is the view itself.
fn view(eye: Eye, x: usize, y: usize) -> u8 {
    (if eye == Left { 1 } else { 100 } + 2 * x + 3 * y) as u8
}

const EW: usize = 8;
const EH: usize = 4;

/// A 4:4:4 frame whose three planes are `f`.
fn frame444(w: usize, h: usize, f: impl Fn(usize, usize) -> u8) -> VideoFrame {
    let p: Vec<u8> = (0..w * h).map(|i| f(i % w, i / w)).collect();
    VideoFrame {
        width: w as u32,
        height: h as u32,
        data: PixelData::Yuv8 { planes: [Arc::new(p.clone()), Arc::new(p.clone()), Arc::new(p)], chroma: Chroma::C444, alpha: None },
        color: ColorInfo::REC709,
        par: (1, 1),
        pts: Tick::ZERO,
    }
}

fn luma(f: &VideoFrame) -> (usize, usize, Vec<u8>) {
    match &f.data {
        PixelData::Yuv8 { planes, .. } => (f.width as usize, f.height as usize, planes[0].to_vec()),
        _ => panic!("not 8-bit Y'CbCr"),
    }
}

/// Checks that `f` is full side by side with each view where it belongs: every sample when
/// `exact`, else the samples away from the edges (the edges have fewer neighbours to fill from).
fn assert_pair(f: &VideoFrame, exact: bool, what: &str) {
    let (w, h, y) = luma(f);
    assert_eq!((w, h), (2 * EW, EH), "{what}: size");
    for (eye, x0) in [(Left, 0), (Right, EW)] {
        for py in 0..EH {
            for px in 0..EW {
                let edge = px == 0 || py == 0 || px == EW - 1 || py == EH - 1;
                if exact || !edge {
                    assert_eq!(y[py * w + x0 + px], view(eye, px, py), "{what}: {eye:?} view at ({px}, {py})");
                }
            }
        }
    }
    match &f.data {
        PixelData::Yuv8 { planes, .. } => assert!(planes[1] == planes[0] && planes[2] == planes[0], "{what}: 4:4:4 chroma follows luma"),
        _ => unreachable!(),
    }
}

/// The view of constituent frame `k` for a pair whose first view is `first`.
fn frame_view(first: Eye, k: usize) -> Eye {
    if k == 0 { first } else { first.other() }
}

#[test]
fn spatial_packings_become_full_side_by_side() {
    let m = StereoMark::InterpretFootage;
    for first in [Left, Right] {
        let v = |k: usize, x: usize, y: usize| view(frame_view(first, k), x, y);
        let layout = |packing, half| StereoLayout { half, ..StereoLayout::new(packing, first, m) };
        let cases: Vec<(StereoLayout, VideoFrame, bool)> = vec![
            (layout(SideBySide, false), frame444(2 * EW, EH, |x, y| if x < EW { v(0, x, y) } else { v(1, x - EW, y) }), true),
            (layout(SideBySide, true), frame444(EW, EH, |x, y| if x < EW / 2 { v(0, 2 * x, y) } else { v(1, 2 * (x - EW / 2), y) }), false),
            (layout(TopBottom, false), frame444(EW, 2 * EH, |x, y| if y < EH { v(0, x, y) } else { v(1, x, y - EH) }), true),
            (layout(TopBottom, true), frame444(EW, EH, |x, y| if y < EH / 2 { v(0, x, 2 * y) } else { v(1, x, 2 * (y - EH / 2)) }), false),
            (layout(RowInterleaved, false), frame444(EW, EH, |x, y| v(y % 2, x, y)), false),
            (layout(ColumnInterleaved, false), frame444(EW, EH, |x, y| v(x % 2, x, y)), false),
            (layout(Checkerboard, false), frame444(EW, EH, |x, y| v((x + y) % 2, x, y)), false),
            (
                // R0: frame 0; R1 | (R2 over R3): frame 1; R4 is not part of either
                layout(TileFormat, false),
                frame444(EW * 3 / 2, EH * 3 / 2, |x, y| {
                    let (tw, th) = (EW / 2, EH / 2);
                    match (x / tw, y / th) {
                        (0 | 1, 0 | 1) => v(0, x, y),
                        (2, _) if y < 2 * th => v(1, x - 2 * tw, y),
                        (0, 2) => v(1, tw + x, y - 2 * th),
                        (1, 2) => v(1, x, th + y - 2 * th),
                        _ => 255,
                    }
                }),
                true,
            ),
        ];
        for (l, packed, exact) in cases {
            assert_pair(&side_by_side(&packed, &l), exact, &format!("{:?} {first:?} first", l.packing));
        }
    }
}

#[test]
fn quincunx_side_by_side_and_top_bottom() {
    // H.264 Figure D-10: frame 0 keeps the samples where x + y is even, frame 1 the others
    let l = StereoLayout { half: true, quincunx: true, ..StereoLayout::new(SideBySide, Left, StereoMark::FramePackingSei) };
    let packed = frame444(EW, EH, |x, y| if x < EW / 2 { view(Left, 2 * x + y % 2, y) } else { view(Right, 2 * (x - EW / 2) + (y + 1) % 2, y) });
    assert_pair(&side_by_side(&packed, &l), false, "quincunx side by side");
    let l = StereoLayout { packing: TopBottom, ..l };
    let packed = frame444(EW, EH, |x, y| if y < EH / 2 { view(Left, x, 2 * y + x % 2) } else { view(Right, x, 2 * (y - EH / 2) + (x + 1) % 2) });
    assert_pair(&side_by_side(&packed, &l), false, "quincunx top-bottom");
}

#[test]
fn a_mirrored_view_is_turned_back() {
    let l = StereoLayout { flipped: Some(Right), ..StereoLayout::new(SideBySide, Left, StereoMark::FramePackingSei) };
    let packed = frame444(2 * EW, EH, |x, y| if x < EW { view(Left, x, y) } else { view(Right, EW - 1 - (x - EW), y) });
    assert_pair(&side_by_side(&packed, &l), true, "right view mirrored left to right");
    let l = StereoLayout { packing: TopBottom, flipped: Some(Left), ..l };
    let packed = frame444(EW, 2 * EH, |x, y| if y < EH { view(Left, x, EH - 1 - y) } else { view(Right, x, y - EH) });
    assert_pair(&side_by_side(&packed, &l), true, "left view mirrored top to bottom");
}

#[test]
fn subsampled_chroma() {
    // 4:2:0 half-width side by side: each view's chroma is upconverted like its luma
    let (w, h) = (16, 8);
    let y: Vec<u8> = (0..w * h).map(|i| if i % w < w / 2 { 10 } else { 200 }).collect();
    let c: Vec<u8> = (0..w / 2 * h / 2).map(|i| if i % (w / 2) < w / 4 { 20 } else { 220 }).collect();
    let f = VideoFrame {
        width: w as u32,
        height: h as u32,
        data: PixelData::Yuv8 { planes: [Arc::new(y), Arc::new(c.clone()), Arc::new(c)], chroma: Chroma::C420, alpha: Some(Arc::new(vec![255; w * h])) },
        color: ColorInfo::REC709,
        par: (1, 1),
        pts: Tick::ZERO,
    };
    let l = StereoLayout { half: true, ..StereoLayout::new(SideBySide, Right, StereoMark::Matroska) };
    let out = side_by_side(&f, &l);
    assert_eq!((out.width, out.height), (32, 8));
    let PixelData::Yuv8 { planes, chroma, alpha } = &out.data else { panic!() };
    assert_eq!(*chroma, Chroma::C420);
    assert_eq!((planes[0].len(), planes[1].len(), alpha.as_ref().map(|a| a.len())), (32 * 8, 16 * 4, Some(32 * 8)));
    // the right view (stored second) is now on the left
    assert!(planes[0].chunks(32).all(|r| r[..16].iter().all(|&v| v == 200) && r[16..].iter().all(|&v| v == 10)));
    assert!(planes[1].chunks(16).all(|r| r[..8].iter().all(|&v| v == 220) && r[8..].iter().all(|&v| v == 20)));
    // 4:2:0 row interleaving: each chroma sample covers rows of both views and stays shared
    let l = StereoLayout::new(RowInterleaved, Left, StereoMark::Matroska);
    let PixelData::Yuv8 { planes: rows, .. } = side_by_side(&f, &l).data else { panic!() };
    assert_eq!(rows[1].len(), 16 * 4);
}

#[test]
fn float_rgba_frames() {
    let px: Vec<f32> = (0..4 * 2).flat_map(|i| if i % 4 < 2 { [0.25; 4] } else { [0.75; 4] }).collect();
    let f = VideoFrame::rgba_f32(4, 2, px);
    let out = side_by_side(&f, &StereoLayout::new(SideBySide, Right, StereoMark::St3d));
    let PixelData::RgbaF32(d) = &out.data else { panic!() };
    assert_eq!((out.width, out.height), (4, 2));
    assert!(d.chunks(16).all(|row| row[..8].iter().all(|&v| v == 0.75) && row[8..].iter().all(|&v| v == 0.25)));
}

// ---- the source

/// A source of numbered frames of its info's size: even frames show the ramp of `even`, odd
/// frames the other one.
struct Alternating {
    info: MediaInfo,
    even: Eye,
}

fn info(w: u32, h: u32, rate: FrameRate) -> MediaInfo {
    MediaInfo {
        name: "t".into(),
        kind: MediaKind::Movie,
        duration: rate.tick_of(10),
        video: Some(VideoStreamInfo {
            width: w,
            height: h,
            frame_rate: rate,
            par: (1, 1),
            codec: "test".into(),
            pixel_format: String::new(),
            color: ColorInfo::REC709,
            has_alpha: false,
            bitrate: None,
            hdr: None,
            stereo: None,
        }),
        audio: None,
        container: String::new(),
        start_timecode: None,
        file_size: None,
    }
}

impl MediaSource for Alternating {
    fn info(&self) -> &MediaInfo {
        &self.info
    }
    fn video_frame(&self, req: FrameRequest) -> Result<Arc<VideoFrame>> {
        let i = self.info.frame_rate().frame_at(req.time);
        let eye = if i % 2 == 0 { self.even } else { self.even.other() };
        let v = self.info.video.as_ref().ok_or(MediaError::NoStream("video"))?;
        Ok(Arc::new(frame444(v.width as usize, v.height as usize, |x, y| view(eye, x, y)).with_pts(self.info.frame_rate().tick_of(i))))
    }
    fn audio(&self, _: i64, _: usize, _: u32) -> Result<AudioBuffer> {
        Err(MediaError::NoStream("audio"))
    }
}

#[test]
fn frame_sequential_pairs_at_half_the_rate() {
    let rate = FrameRate::FPS_50;
    for first in [Left, Right] {
        let src = SideBySideSource::new(
            Arc::new(Alternating { info: info(EW as u32, EH as u32, rate), even: first }),
            StereoLayout::new(FrameSequential, first, StereoMark::Matroska),
        );
        let v = src.info().video.clone().unwrap();
        assert_eq!((v.width, v.height, v.frame_rate), (2 * EW as u32, EH as u32, FrameRate::FPS_25));
        assert_eq!(v.stereo.map(|s| s.packing), Some(FrameSequential));
        // pair 1 is stream frames 2 and 3, wherever in it the request falls
        for t in [FrameRate::FPS_25.tick_of(1), rate.tick_of(3)] {
            let f = src.video_frame(FrameRequest::full(t)).unwrap();
            let (w, _, y) = luma(&f);
            assert_eq!(w, 2 * EW);
            // the first frame of a pair is `first`; the left view is always on the left
            assert_eq!((y[0], y[EW]), (view(Left, 0, 0), view(Right, 0, 0)), "{first:?} first");
            assert_eq!(f.pts, rate.tick_of(if first == Left { 2 } else { 3 }));
        }
    }
}

#[test]
fn wrapping() {
    let src: SharedSource = Arc::new(Alternating { info: info(2 * EW as u32, EH as u32, FrameRate::FPS_25), even: Left });
    // no layout: the source itself
    assert!(Arc::ptr_eq(&wrap(src.clone(), None), &src));
    // a layout that doesn't separate two views: the frames as they are, the layout recorded
    for l in [StereoLayout::from_matroska(0), StereoLayout::from_matroska(10), StereoLayout::from_st3d(3)] {
        let w = wrap(src.clone(), l);
        assert_eq!(w.info().video.as_ref().map(|v| (v.width, v.frame_rate, v.stereo)), Some((2 * EW as u32, FrameRate::FPS_25, l)));
        assert_eq!(luma(&w.video_frame(FrameRequest::full(Tick::ZERO)).unwrap()).2, luma(&src.video_frame(FrameRequest::full(Tick::ZERO)).unwrap()).2);
    }
    // already full side by side with the left eye first: the frames pass through untouched
    let w = SideBySideSource::new(src.clone(), StereoLayout::full_side_by_side(StereoMark::St3d));
    let a = src.video_frame(FrameRequest::full(Tick::ZERO)).unwrap();
    let b = w.video_frame(FrameRequest::full(Tick::ZERO)).unwrap();
    assert_eq!(luma(&a).2, luma(&b).2);
    assert_eq!(w.info().video.as_ref().map(|v| (v.width, v.stereo.is_some())), Some((2 * EW as u32, true)));
    // a half-width pair doubles the width it reports
    let w = wrap(src, Some(StereoLayout { half: true, ..StereoLayout::from_matroska(1).unwrap() }));
    assert_eq!(w.info().video.as_ref().map(|v| v.width), Some(4 * EW as u32));
    assert_eq!(luma(&w.video_frame(FrameRequest::full(Tick::ZERO)).unwrap()).0, 4 * EW);
}

/// Every layout over tiny, odd and inconsistent frames: no panic, and the delivered size
/// [`StereoLayout::delivered_size`] promises (AGENTS.md §0.4).
#[test]
fn hostile_frames_never_panic() {
    let mut layouts = StereoLayout::choices();
    layouts.push(StereoLayout { half: true, quincunx: true, flipped: Some(Right), ..StereoLayout::from_matroska(1).unwrap() });
    layouts.push(StereoLayout { half: true, quincunx: true, flipped: Some(Left), ..StereoLayout::from_matroska(3).unwrap() });
    for l in layouts {
        for (w, h) in [(0usize, 0usize), (1, 1), (1, 3), (2, 1), (3, 3), (5, 2), (7, 7), (9, 6)] {
            for short in [false, true] {
                // planes shorter than the frame says (a damaged decoder output)
                let n = if short { (w * h) / 2 } else { w * h };
                let cn = if short { 0 } else { w.div_ceil(2) * h.div_ceil(2) };
                let f = VideoFrame {
                    width: w as u32,
                    height: h as u32,
                    data: PixelData::Yuv8 { planes: [Arc::new(vec![9; n]), Arc::new(vec![9; cn]), Arc::new(vec![9; cn])], chroma: Chroma::C420, alpha: None },
                    color: ColorInfo::REC709,
                    par: (1, 1),
                    pts: Tick::ZERO,
                };
                let out = std::panic::catch_unwind(|| side_by_side(&f, &l)).unwrap_or_else(|_| panic!("{l:?} {w}x{h} short {short}"));
                if l.is_stereo() && l.packing != FrameSequential && !short {
                    assert_eq!((out.width, out.height), l.delivered_size(w as u32, h as u32), "{l:?} {w}x{h}");
                }
                let g = VideoFrame::rgba_f32(w as u32, h as u32, vec![0.5; w * h * 4]);
                assert!(std::panic::catch_unwind(|| side_by_side(&g, &l)).is_ok(), "{l:?} {w}x{h} RGBA");
            }
        }
    }
    // frames of different formats or heights are not joined
    let a = frame444(4, 2, |_, _| 1);
    let b = VideoFrame::rgba_f32(4, 2, vec![0.0; 32]);
    assert_eq!(join_frames(&a, &b).width, 4);
    assert_eq!(join_frames(&a, &frame444(4, 3, |_, _| 1)).width, 4);
}

#[test]
fn interpret_footage_choices() {
    let ids: Vec<String> = StereoLayout::choices().iter().map(StereoLayout::id).collect();
    assert_eq!(ids.len(), 19);
    for id in [
        "2d",
        "sbs-left",
        "sbs-right",
        "sbs-half-left",
        "sbs-half-right",
        "tb-half-right",
        "rows-left",
        "columns-right",
        "checkerboard-left",
        "frames-right",
        "tiles-left",
    ] {
        assert!(ids.contains(&id.to_string()), "{id}");
    }
    for l in StereoLayout::choices() {
        assert_eq!(StereoLayout::from_id(&l.id()), Some(l));
        assert_eq!(l.mark, StereoMark::InterpretFootage);
    }
    assert_eq!(StereoLayout::from_id("sideways"), None);
}

#[test]
fn layout_labels() {
    assert_eq!(
        StereoLayout { half: true, ..StereoLayout::from_matroska(1).unwrap() }.label(),
        "Side by side, half width, left eye first (Matroska StereoMode)"
    );
    assert_eq!(StereoLayout::from_frame_packing(5, 2, false, false, false, true).unwrap().label(), "Frame sequential, right eye first (frame packing SEI)");
    assert_eq!(StereoLayout::from_st3d(0).unwrap().label(), "2D (st3d box)");
}
