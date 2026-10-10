//! Stereo 3D video: how a stream packs the two views of a stereo pair, and the source that
//! delivers each pair as one picture, full side by side with the left eye first.
//!
//! - [`StereoLayout`]: the layout of one video stream, read (by the codecs layer) from the marks
//!   the specifications define: the frame packing arrangement SEI messages of H.264 (V15)
//!   08/2024 (D.2.26, Tables D-9 and D-10) and H.265 (V10) 07/2024 (D.3.16, Tables D.8 and D.9;
//!   the segmented rectangular one, D.3.29), Matroska `StereoMode` (RFC 9559 Table 5) and the Spherical Video V2 `st3d` box.
//!   Interpret Footage can replace it.
//! - [`SideBySideSource`]: wraps a source whose frames hold a stereo pair in any of those layouts
//!   and delivers every pair as one frame: the left view in the left half, the right view in the
//!   right half, each at full resolution. A frame-sequential stream delivers one frame per pair,
//!   at half its frame rate.
//!
//! Views stored at reduced resolution (half-width side by side, half-height top-bottom, row,
//! column and checkerboard interleaving) are upconverted to the frame size, the rearrangement the
//! H.264 and H.265 figures D-1 to D-10 illustrate. Missing samples are the mean of the view's own
//! neighbouring samples (left and right, above and below, or all four for quincunx sampling:
//! the example H.264 NOTE 1 of D.2.26 gives). The specifications leave the upconversion itself
//! open, and the sampling grid positions are not used.

use std::sync::Arc;

use filmcraft_frame::{AudioBuffer, PixelData, VideoFrame};
use filmcraft_time::FrameRate;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{FrameRequest, MediaInfo, MediaSource, Result, SharedSource};

/// One view of a stereo pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Eye {
    Left,
    Right,
}

impl Eye {
    pub fn other(self) -> Eye {
        match self {
            Eye::Left => Eye::Right,
            Eye::Right => Eye::Left,
        }
    }
}

/// How the two views share a video stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StereoPacking {
    /// One view only: H.264 frame packing type 6, Matroska `StereoMode` 0, `st3d` 0.
    Mono,
    /// The views side by side (H.264/H.265 type 3; `StereoMode` 1, 11; `st3d` 2, 4).
    SideBySide,
    /// One view above the other (H.264/H.265 type 4; `StereoMode` 2, 3; `st3d` 1).
    TopBottom,
    /// Alternate rows (H.264 type 2; `StereoMode` 6, 7).
    RowInterleaved,
    /// Alternate columns (H.264 type 1; `StereoMode` 8, 9).
    ColumnInterleaved,
    /// A checkerboard of quincunx-sampled views (H.264 type 0; `StereoMode` 4, 5).
    Checkerboard,
    /// Alternate frames (H.264/H.265 type 5; `StereoMode` 13, 14: both eyes laced in one block).
    FrameSequential,
    /// A full-resolution view in the top-left two thirds by two thirds of the frame and the
    /// other view in the remaining regions (H.264 type 7, H.265 segmented rectangular frame
    /// packing: a 1280×720 pair in a 1920×1080 frame).
    TileFormat,
    /// A colour anaglyph (`StereoMode` 10): the views are mixed in one picture and can't be
    /// separated.
    AnaglyphCyanRed,
    /// A colour anaglyph (`StereoMode` 12).
    AnaglyphGreenMagenta,
    /// Two views laid out by other means (`st3d` 3, stereo-custom, e.g. per-eye projection
    /// meshes).
    Custom,
}

/// Where a stream's stereo layout comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StereoMark {
    /// An H.264 or H.265 frame packing arrangement SEI message in the bitstream.
    FramePackingSei,
    /// The Matroska track's `StereoMode`.
    Matroska,
    /// The Spherical Video V2 `st3d` box of an MP4 / QuickTime sample entry.
    St3d,
    /// Set by hand (Interpret Footage).
    InterpretFootage,
}

/// The stereo layout of a video stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StereoLayout {
    pub packing: StereoPacking,
    /// The view stored first: in the left half, the top half, the even rows or columns, the
    /// checkerboard's top-left sample, the top-left tile region, or the first frame of a pair.
    pub first: Eye,
    /// Side by side and top-bottom: each view is stored at half resolution across the packing and
    /// is shown upconverted to the frame size (else each view is half of the frame as stored).
    #[serde(default)]
    pub half: bool,
    /// Half-resolution side-by-side or top-bottom views are quincunx sampled (H.264 / H.265
    /// `quincunx_sampling_flag`; H.264 Figure D-10).
    #[serde(default)]
    pub quincunx: bool,
    /// The view stored mirrored: horizontally for side by side, vertically for top-bottom
    /// (`spatial_flipping_flag`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flipped: Option<Eye>,
    pub mark: StereoMark,
}

impl StereoLayout {
    /// A layout with full-resolution, unmirrored views.
    pub fn new(packing: StereoPacking, first: Eye, mark: StereoMark) -> Self {
        Self { packing, first, half: false, quincunx: false, flipped: None, mark }
    }

    /// Full side by side with the left eye first: what [`SideBySideSource`] delivers and what
    /// FilmCraft writes.
    pub fn full_side_by_side(mark: StereoMark) -> Self {
        Self::new(StereoPacking::SideBySide, Eye::Left, mark)
    }

    /// The two views can be told apart (not mono, an anaglyph or a custom layout).
    pub fn is_stereo(&self) -> bool {
        !matches!(self.packing, StereoPacking::Mono | StereoPacking::AnaglyphCyanRed | StereoPacking::AnaglyphGreenMagenta | StereoPacking::Custom)
    }

    /// From an H.264 or H.265 frame packing arrangement SEI message: `arrangement_type` (H.264
    /// Table D-9: 0 checkerboard … 7 tile format; H.265 Table D.8 defines 3, 4 and 5),
    /// `content_interpretation_type` (Table D-10 / D.9: 2 when constituent frame 0 is the right
    /// view; 1, and the unspecified 0, are read as frame 0 being the left view), the quincunx
    /// sampling and spatial flipping flags, and `current_frame_is_frame0_flag` of the first frame
    /// (type 5: whether the stream starts with frame 0). Reserved values give None, as both
    /// specifications tell decoders to ignore such messages. `half` is left for the caller.
    pub fn from_frame_packing(
        arrangement_type: u8,
        content_interpretation_type: u8,
        quincunx: bool,
        spatial_flipping: bool,
        frame0_flipped: bool,
        current_frame_is_frame0: bool,
    ) -> Option<Self> {
        use StereoPacking::*;
        let packing = match arrangement_type {
            0 => Checkerboard,
            1 => ColumnInterleaved,
            2 => RowInterleaved,
            3 => SideBySide,
            4 => TopBottom,
            5 => FrameSequential,
            6 => Mono,
            7 => TileFormat,
            _ => return None,
        };
        // constituent frame 0: the one with the decoded frame's upper-left sample (types other
        // than 5), the first frame of each pair (type 5)
        let frame0 = match content_interpretation_type {
            0 | 1 => Eye::Left,
            2 => Eye::Right,
            _ => return None,
        };
        let first = if packing == FrameSequential && !current_frame_is_frame0 { frame0.other() } else { frame0 };
        let mut l = Self::new(packing, first, StereoMark::FramePackingSei);
        if matches!(packing, SideBySide | TopBottom) {
            l.quincunx = quincunx;
            // quincunx-sampled views are always upconverted
            l.half = quincunx;
            if spatial_flipping {
                l.flipped = Some(if frame0_flipped { frame0 } else { frame0.other() });
            }
        }
        Some(l)
    }

    /// From a Matroska `StereoMode` (RFC 9559 Table 5). Values not in the table give None.
    pub fn from_matroska(stereo_mode: u64) -> Option<Self> {
        use Eye::*;
        use StereoPacking::*;
        let (packing, first) = match stereo_mode {
            0 => (Mono, Left),
            1 => (SideBySide, Left),
            2 => (TopBottom, Right),
            3 => (TopBottom, Left),
            4 => (Checkerboard, Right),
            5 => (Checkerboard, Left),
            6 => (RowInterleaved, Right),
            7 => (RowInterleaved, Left),
            8 => (ColumnInterleaved, Right),
            9 => (ColumnInterleaved, Left),
            10 => (AnaglyphCyanRed, Left),
            11 => (SideBySide, Right),
            12 => (AnaglyphGreenMagenta, Left),
            13 => (FrameSequential, Left),
            14 => (FrameSequential, Right),
            _ => return None,
        };
        Some(Self::new(packing, first, StereoMark::Matroska))
    }

    /// From the `stereo_mode` of a Spherical Video V2 `st3d` box: 0 monoscopic, 1 top-bottom
    /// (left eye on top), 2 left-right, 3 stereo-custom, 4 right-left. Other values give None.
    /// Each eye is half of the frame as stored (`st3d` has no upconversion).
    pub fn from_st3d(stereo_mode: u8) -> Option<Self> {
        use Eye::*;
        use StereoPacking::*;
        let (packing, first) = match stereo_mode {
            0 => (Mono, Left),
            1 => (TopBottom, Left),
            2 => (SideBySide, Left),
            3 => (Custom, Left),
            4 => (SideBySide, Right),
            _ => return None,
        };
        Some(Self::new(packing, first, StereoMark::St3d))
    }

    /// The Matroska `StereoMode` for this layout (RFC 9559 Table 5), if it has one.
    pub fn to_matroska(&self) -> Option<u64> {
        use Eye::*;
        use StereoPacking::*;
        Some(match (self.packing, self.first) {
            (Mono, _) => 0,
            (SideBySide, Left) => 1,
            (TopBottom, Right) => 2,
            (TopBottom, Left) => 3,
            (Checkerboard, Right) => 4,
            (Checkerboard, Left) => 5,
            (RowInterleaved, Right) => 6,
            (RowInterleaved, Left) => 7,
            (ColumnInterleaved, Right) => 8,
            (ColumnInterleaved, Left) => 9,
            (AnaglyphCyanRed, _) => 10,
            (SideBySide, Right) => 11,
            (AnaglyphGreenMagenta, _) => 12,
            (FrameSequential, Left) => 13,
            (FrameSequential, Right) => 14,
            (TileFormat | Custom, _) => return None,
        })
    }

    /// The `st3d` `stereo_mode` for this layout, if it has one (`st3d` has no top-bottom with the
    /// right eye on top, and no interleaving).
    pub fn to_st3d(&self) -> Option<u8> {
        use Eye::*;
        use StereoPacking::*;
        match (self.packing, self.first) {
            (Mono, _) => Some(0),
            (TopBottom, Left) => Some(1),
            (SideBySide, Left) => Some(2),
            (Custom, _) => Some(3),
            (SideBySide, Right) => Some(4),
            _ => None,
        }
    }

    /// The H.264 frame packing arrangement (`frame_packing_arrangement_type`,
    /// `content_interpretation_type`) for this layout, if it has one.
    pub fn to_frame_packing(&self) -> Option<(u8, u8)> {
        use StereoPacking::*;
        let t = match self.packing {
            Checkerboard => 0,
            ColumnInterleaved => 1,
            RowInterleaved => 2,
            SideBySide => 3,
            TopBottom => 4,
            FrameSequential => 5,
            Mono => return Some((6, 0)),
            TileFormat => 7,
            AnaglyphCyanRed | AnaglyphGreenMagenta | Custom => return None,
        };
        Some((t, if self.first == Eye::Left { 1 } else { 2 }))
    }

    /// The size of one view at full resolution in a `w`×`h` frame of this layout.
    pub fn eye_size(&self, w: u32, h: u32) -> (u32, u32) {
        use StereoPacking::*;
        match self.packing {
            SideBySide if !self.half => (w / 2, h),
            TopBottom if !self.half => (w, h / 2),
            TileFormat => (w / 3 * 2, h / 3 * 2),
            _ => (w, h),
        }
    }

    /// The size of the full side-by-side picture [`SideBySideSource`] delivers for a `w`×`h`
    /// stream of this layout.
    pub fn delivered_size(&self, w: u32, h: u32) -> (u32, u32) {
        let (ew, eh) = self.eye_size(w, h);
        (ew * 2, eh)
    }

    /// Every layout Interpret Footage offers, by [`StereoLayout::id`]: 2D, then each packing
    /// with each view first (and half resolution for side by side and top-bottom).
    pub fn choices() -> Vec<StereoLayout> {
        use StereoPacking::*;
        let mut v = vec![StereoLayout::new(Mono, Eye::Left, StereoMark::InterpretFootage)];
        for packing in [SideBySide, TopBottom, RowInterleaved, ColumnInterleaved, Checkerboard, FrameSequential, TileFormat] {
            for half in if matches!(packing, SideBySide | TopBottom) { vec![false, true] } else { vec![false] } {
                for first in [Eye::Left, Eye::Right] {
                    v.push(StereoLayout { half, ..StereoLayout::new(packing, first, StereoMark::InterpretFootage) });
                }
            }
        }
        v
    }

    /// A stable id for the layouts of [`StereoLayout::choices`]: `2d`, `sbs-left`,
    /// `sbs-half-right`, `tb-left`, `rows-left`, `columns-right`, `checkerboard-left`,
    /// `frames-left`, `tiles-right`, … (anaglyph and custom layouts: `anaglyph-cyan-red`,
    /// `anaglyph-green-magenta`, `custom`).
    pub fn id(&self) -> String {
        use StereoPacking::*;
        let p = match self.packing {
            Mono => return "2d".into(),
            AnaglyphCyanRed => return "anaglyph-cyan-red".into(),
            AnaglyphGreenMagenta => return "anaglyph-green-magenta".into(),
            Custom => return "custom".into(),
            SideBySide => "sbs",
            TopBottom => "tb",
            RowInterleaved => "rows",
            ColumnInterleaved => "columns",
            Checkerboard => "checkerboard",
            FrameSequential => "frames",
            TileFormat => "tiles",
        };
        let half = if self.half && matches!(self.packing, SideBySide | TopBottom) { "-half" } else { "" };
        format!("{p}{half}-{}", if self.first == Eye::Left { "left" } else { "right" })
    }

    /// The Interpret Footage layout with this [`StereoLayout::id`].
    pub fn from_id(id: &str) -> Option<StereoLayout> {
        Self::choices().into_iter().find(|l| l.id() == id)
    }

    /// A short description with where it comes from: "Side by side, half width, left eye first
    /// (frame packing SEI)".
    pub fn label(&self) -> String {
        format!("{} ({})", self.describe(), self.mark.label())
    }

    /// A short description: "Side by side, half width, left eye first".
    pub fn describe(&self) -> String {
        use StereoPacking::*;
        let what = match self.packing {
            Mono => return "2D".into(),
            AnaglyphCyanRed => return "Anaglyph, cyan/red".into(),
            AnaglyphGreenMagenta => return "Anaglyph, green/magenta".into(),
            Custom => return "Stereo, custom layout".into(),
            SideBySide if self.half => "Side by side, half width",
            SideBySide => "Side by side",
            TopBottom if self.half => "Top-bottom, half height",
            TopBottom => "Top-bottom",
            RowInterleaved => "Row interleaved",
            ColumnInterleaved => "Column interleaved",
            Checkerboard => "Checkerboard",
            FrameSequential => "Frame sequential",
            TileFormat => "Tile format",
        };
        let eye = if self.first == Eye::Left { "left eye first" } else { "right eye first" };
        format!("{what}, {eye}")
    }
}

impl StereoMark {
    pub fn label(self) -> &'static str {
        match self {
            StereoMark::FramePackingSei => "frame packing SEI",
            StereoMark::Matroska => "Matroska StereoMode",
            StereoMark::St3d => "st3d box",
            StereoMark::InterpretFootage => "Interpret Footage",
        }
    }
}

/// Whether the views of a side-by-side or top-bottom `w`×`h` frame with pixel aspect `par` are
/// read as stored at half resolution when nothing else says: when full-resolution views would be
/// narrower than 5:4 (side by side) or wider than 5:2 (top-bottom). Pictures from 4:3 to 2.39:1
/// fall in between, so a 1920×1080 side-by-side frame holds two squeezed 16:9 views and a
/// 3840×1080 one two full 16:9 views. This is FilmCraft's reading, not a rule of the
/// specifications; Interpret Footage overrides it.
pub fn half_by_shape(packing: StereoPacking, w: u32, h: u32, par: (u32, u32)) -> bool {
    if w == 0 || h == 0 || par.0 == 0 || par.1 == 0 {
        return false;
    }
    let dar = w as f64 * par.0 as f64 / (h as f64 * par.1 as f64);
    match packing {
        StereoPacking::SideBySide => dar / 2.0 < 1.25,
        StereoPacking::TopBottom => dar * 2.0 > 2.5,
        _ => false,
    }
}

/// `src` read with `layout`: full side by side when the layout separates two views; for other
/// layouts (2D, anaglyph, custom) the frames as they are, with `layout` recorded in the stream
/// info. `src` itself when there is nothing to change (no layout, or the stream's own
/// non-separable mark).
pub fn wrap(src: SharedSource, layout: Option<StereoLayout>) -> SharedSource {
    let Some(l) = layout else { return src };
    let own = src.info().video.as_ref().map(|v| v.stereo);
    match own {
        None => src,
        Some(own) if !l.is_stereo() && own == Some(l) => src,
        Some(_) => Arc::new(SideBySideSource::new(src, l)),
    }
}

/// A source delivering the stereo pairs of another source as full side by side pictures, the left
/// view on the left (see the module documentation).
pub struct SideBySideSource {
    inner: SharedSource,
    layout: StereoLayout,
    /// The inner source's frame rate (frame-sequential pairs are two of its frames).
    stream_rate: FrameRate,
    info: MediaInfo,
}

impl SideBySideSource {
    /// Wrap `inner`, whose video stream info gives the stored frame size and rate, for `layout`
    /// (a layout that doesn't separate two views passes the frames through).
    pub fn new(inner: SharedSource, layout: StereoLayout) -> Self {
        let mut info = inner.info().clone();
        let stream_rate = info.frame_rate();
        if let Some(v) = info.video.as_mut() {
            if layout.is_stereo() {
                (v.width, v.height) = layout.delivered_size(v.width, v.height);
                if layout.packing == StereoPacking::FrameSequential {
                    v.frame_rate = FrameRate::new(v.frame_rate.num, v.frame_rate.den.saturating_mul(2));
                }
            }
            v.stereo = Some(layout);
        }
        Self { inner, layout, stream_rate, info }
    }

    pub fn layout(&self) -> StereoLayout {
        self.layout
    }
}

impl MediaSource for SideBySideSource {
    fn info(&self) -> &MediaInfo {
        &self.info
    }

    fn video_frame(&self, req: FrameRequest) -> Result<Arc<VideoFrame>> {
        if !self.layout.is_stereo() {
            return self.inner.video_frame(req);
        }
        if self.layout.packing == StereoPacking::FrameSequential {
            // pair p: frames 2p and 2p + 1 of the stream
            let first = self.stream_rate.frame_at(req.time).div_euclid(2) * 2;
            let a = self.inner.video_frame(FrameRequest { time: self.stream_rate.tick_of(first), ..req })?;
            let b = self.inner.video_frame(FrameRequest { time: self.stream_rate.tick_of(first + 1), ..req })?;
            let (l, r) = if self.layout.first == Eye::Left { (&a, &b) } else { (&b, &a) };
            return Ok(Arc::new(join_frames(l, r)));
        }
        let f = self.inner.video_frame(req)?;
        let l = &self.layout;
        if l.packing == StereoPacking::SideBySide && l.first == Eye::Left && !l.half && l.flipped.is_none() && f.width.is_multiple_of(2) {
            // already full side by side, left eye first
            return Ok(f);
        }
        Ok(Arc::new(side_by_side(&f, l)))
    }

    fn audio(&self, start: i64, frames: usize, sample_rate: u32) -> Result<AudioBuffer> {
        self.inner.audio(start, frames, sample_rate)
    }
}

/// A sample type of a frame plane.
trait Sample: Copy + Default + Send + Sync {
    fn to_f32(self) -> f32;
    fn from_f32(v: f32) -> Self;
}

impl Sample for u8 {
    fn to_f32(self) -> f32 {
        self as f32
    }
    fn from_f32(v: f32) -> Self {
        v.round().clamp(0.0, 255.0) as u8
    }
}

impl Sample for u16 {
    fn to_f32(self) -> f32 {
        self as f32
    }
    fn from_f32(v: f32) -> Self {
        v.round().clamp(0.0, 65535.0) as u16
    }
}

impl Sample for f32 {
    fn to_f32(self) -> f32 {
        self
    }
    fn from_f32(v: f32) -> Self {
        v
    }
}

/// A `w`×`h` plane of `n`-channel samples.
#[derive(Clone)]
struct Plane<T> {
    px: Vec<T>,
    w: usize,
    h: usize,
    n: usize,
}

/// Which neighbours fill a sample a view doesn't have.
#[derive(Clone, Copy)]
enum Fill {
    /// Left and right.
    Across,
    /// Above and below.
    Down,
    /// All four (quincunx sampling).
    Around,
}

impl<T: Sample> Plane<T> {
    fn new(px: &[T], w: usize, h: usize, n: usize) -> Self {
        let mut px = px.to_vec();
        px.resize(w * h * n, T::default());
        Self { px, w, h, n }
    }

    /// The rectangle at (`x0`, `y0`), cut to the plane.
    fn crop(&self, x0: usize, y0: usize, w: usize, h: usize) -> Self {
        let n = self.n;
        let (x0, y0) = (x0.min(self.w), y0.min(self.h));
        let (w, h) = (w.min(self.w - x0), h.min(self.h - y0));
        let mut px = Vec::with_capacity(w * h * n);
        for y in y0..y0 + h {
            let s = (y * self.w + x0) * n;
            px.extend_from_slice(&self.px[s..s + w * n]);
        }
        Self { px, w, h, n }
    }

    /// `a` and `b` next to each other (`b` on the right), cut to the lower height.
    fn beside(a: &Self, b: &Self) -> Self {
        let (n, h, w) = (a.n, a.h.min(b.h), a.w + b.w);
        let mut px = Vec::with_capacity(w * h * n);
        for y in 0..h {
            px.extend_from_slice(&a.px[y * a.w * n..(y + 1) * a.w * n]);
            px.extend_from_slice(&b.px[y * b.w * n..(y + 1) * b.w * n]);
        }
        Self { px, w, h, n }
    }

    /// `a` above `b`, cut to the lower width.
    fn above(a: &Self, b: &Self) -> Self {
        let (n, w) = (a.n, a.w.min(b.w));
        let mut px = Vec::with_capacity(w * (a.h + b.h) * n);
        for p in [a, b] {
            for y in 0..p.h {
                px.extend_from_slice(&p.px[y * p.w * n..(y * p.w + w) * n]);
            }
        }
        Self { px, w, h: a.h + b.h, n }
    }

    /// The plane mirrored left to right (`horizontal`) or top to bottom.
    fn mirrored(&self, horizontal: bool) -> Self {
        let n = self.n;
        let mut px = Vec::with_capacity(self.px.len());
        for y in 0..self.h {
            let sy = if horizontal { y } else { self.h - 1 - y };
            for x in 0..self.w {
                let sx = if horizontal { self.w - 1 - x } else { x };
                let s = (sy * self.w + sx) * n;
                px.extend_from_slice(&self.px[s..s + n]);
            }
        }
        Self { px, w: self.w, h: self.h, n }
    }

    /// A `w`×`h` view: the samples at the positions where `has` is true are this plane's samples
    /// at `at(x, y)`; the others are the mean of the view's neighbouring samples that `fill`
    /// names (each of them is a position the view has).
    fn upconvert(&self, w: usize, h: usize, has: impl Fn(usize, usize) -> bool + Sync, at: impl Fn(usize, usize) -> (usize, usize) + Sync, fill: Fill) -> Self {
        let n = self.n;
        let mut px = vec![T::default(); w * h * n];
        let src = |x: usize, y: usize| {
            let (sx, sy) = at(x, y);
            let i = (sy.min(self.h.saturating_sub(1)) * self.w + sx.min(self.w.saturating_sub(1))) * n;
            &self.px[i..i + n]
        };
        if self.w == 0 || self.h == 0 {
            return Self { px, w, h, n };
        }
        px.par_chunks_mut((w * n).max(1)).enumerate().for_each(|(y, row)| {
            for x in 0..w {
                let out = &mut row[x * n..x * n + n];
                if has(x, y) {
                    out.copy_from_slice(src(x, y));
                    continue;
                }
                let mut near: [Option<(usize, usize)>; 4] = [None; 4];
                if matches!(fill, Fill::Across | Fill::Around) {
                    near[0] = x.checked_sub(1).map(|x| (x, y));
                    near[1] = (x + 1 < w).then_some((x + 1, y));
                }
                if matches!(fill, Fill::Down | Fill::Around) {
                    near[2] = y.checked_sub(1).map(|y| (x, y));
                    near[3] = (y + 1 < h).then_some((x, y + 1));
                }
                let near: Vec<&[T]> = near.iter().flatten().filter(|(x, y)| has(*x, *y)).map(|(x, y)| src(*x, *y)).collect();
                for (c, o) in out.iter_mut().enumerate() {
                    let sum: f32 = near.iter().map(|s| s[c].to_f32()).sum();
                    *o = T::from_f32(sum / near.len().max(1) as f32);
                }
            }
        });
        Self { px, w, h, n }
    }
}

/// Constituent frame `k` (0: the one stored first) of a plane packed as `l`. `shift`: the
/// plane's chroma subsampling (0 for luma, alpha and RGBA). A chroma plane subsampled across an
/// interleaving holds samples shared by both views and is kept whole for each.
fn constituent<T: Sample>(p: &Plane<T>, shift: (u32, u32), l: &StereoLayout, k: usize) -> Plane<T> {
    use StereoPacking::*;
    let (w, h) = (p.w, p.h);
    match l.packing {
        SideBySide => {
            let hw = w / 2;
            let x0 = k * hw;
            // a half-width view's sample (x / 2), kept inside its own half
            let at = move |x: usize, y: usize| (x0 + (x / 2).min(hw.saturating_sub(1)), y);
            if !l.half {
                p.crop(x0, 0, hw, h)
            } else if l.quincunx {
                p.upconvert(w, h, |x, y| (x + y + k).is_multiple_of(2), at, Fill::Around)
            } else {
                p.upconvert(w, h, |x, _| x.is_multiple_of(2), at, Fill::Across)
            }
        }
        TopBottom => {
            let hh = h / 2;
            let y0 = k * hh;
            let at = move |x: usize, y: usize| (x, y0 + (y / 2).min(hh.saturating_sub(1)));
            if !l.half {
                p.crop(0, y0, w, hh)
            } else if l.quincunx {
                p.upconvert(w, h, |x, y| (x + y + k).is_multiple_of(2), at, Fill::Around)
            } else {
                p.upconvert(w, h, |_, y| y.is_multiple_of(2), at, Fill::Down)
            }
        }
        RowInterleaved if shift.1 == 0 => p.upconvert(w, h, |_, y| y % 2 == k, |x, y| (x, y), Fill::Down),
        ColumnInterleaved if shift.0 == 0 => p.upconvert(w, h, |x, _| x % 2 == k, |x, y| (x, y), Fill::Across),
        Checkerboard if shift == (0, 0) => p.upconvert(w, h, |x, y| (x + y) % 2 == k, |x, y| (x, y), Fill::Around),
        TileFormat => {
            let (tw, th) = (w / 3, h / 3);
            if k == 0 {
                p.crop(0, 0, 2 * tw, 2 * th)
            } else {
                Plane::beside(&p.crop(2 * tw, 0, tw, 2 * th), &Plane::above(&p.crop(0, 2 * th, tw, th), &p.crop(tw, 2 * th, tw, th)))
            }
        }
        _ => p.clone(),
    }
}

/// One plane of a frame packed as `l`, as full side by side with the left eye first.
fn pair_plane<T: Sample>(p: &Plane<T>, shift: (u32, u32), l: &StereoLayout) -> Plane<T> {
    let frames = [0, 1].map(|k| {
        let f = constituent(p, shift, l, k);
        let eye = if k == 0 { l.first } else { l.first.other() };
        if l.flipped == Some(eye) { f.mirrored(l.packing == StereoPacking::SideBySide) } else { f }
    });
    let [a, b] = &frames;
    if l.first == Eye::Left { Plane::beside(a, b) } else { Plane::beside(b, a) }
}

/// The planes of a planar Y'CbCr frame (Y, Cb, Cr, then alpha) with their chroma subsampling
/// shifts.
fn planes<T: Sample>(p: &[Arc<Vec<T>>; 3], alpha: &Option<Arc<Vec<T>>>, w: usize, h: usize, shifts: (u32, u32)) -> Vec<(Plane<T>, (u32, u32))> {
    let (cw, ch) = (w.div_ceil(1 << shifts.0), h.div_ceil(1 << shifts.1));
    let mut v = vec![(Plane::new(&p[0], w, h, 1), (0, 0)), (Plane::new(&p[1], cw, ch, 1), shifts), (Plane::new(&p[2], cw, ch, 1), shifts)];
    if let Some(a) = alpha {
        v.push((Plane::new(a, w, h, 1), (0, 0)));
    }
    v
}

/// Planar Y'CbCr planes after `op`: ([Y, Cb, Cr], alpha, width, height).
type YuvOut<T> = ([Arc<Vec<T>>; 3], Option<Arc<Vec<T>>>, usize, usize);

fn map_yuv<T: Sample>(
    p: &[Arc<Vec<T>>; 3],
    alpha: &Option<Arc<Vec<T>>>,
    w: usize,
    h: usize,
    shifts: (u32, u32),
    op: impl Fn(&Plane<T>, (u32, u32), usize) -> Plane<T>,
) -> YuvOut<T> {
    let mut out: Vec<Plane<T>> = planes(p, alpha, w, h, shifts).iter().enumerate().map(|(i, (p, s))| op(p, *s, i)).collect();
    let a = (out.len() > 3).then(|| Arc::new(std::mem::take(&mut out[3].px)));
    let (ow, oh) = (out[0].w, out[0].h);
    let mut it = out.into_iter().map(|p| Arc::new(p.px));
    let mut next = || it.next().unwrap_or_default();
    ([next(), next(), next()], a, ow, oh)
}

fn with_data(like: &VideoFrame, data: PixelData, w: usize, h: usize) -> VideoFrame {
    VideoFrame { width: w as u32, height: h as u32, data, color: like.color, par: like.par, pts: like.pts }
}

/// A frame holding a stereo pair packed as `layout` (any spatial packing), rearranged as full
/// side by side with the left view on the left. Frames of the other packings come back as they
/// are.
pub fn side_by_side(frame: &VideoFrame, layout: &StereoLayout) -> VideoFrame {
    if !layout.is_stereo() || layout.packing == StereoPacking::FrameSequential {
        return frame.clone();
    }
    let (w, h) = (frame.width as usize, frame.height as usize);
    match &frame.data {
        PixelData::Rgba8(d) => {
            let p = pair_plane(&Plane::new(d, w, h, 4), (0, 0), layout);
            with_data(frame, PixelData::Rgba8(Arc::new(p.px)), p.w, p.h)
        }
        PixelData::RgbaF32(d) => {
            let p = pair_plane(&Plane::new(d, w, h, 4), (0, 0), layout);
            with_data(frame, PixelData::RgbaF32(Arc::new(p.px)), p.w, p.h)
        }
        PixelData::Yuv8 { planes, chroma, alpha } => {
            let (planes, alpha, ow, oh) = map_yuv(planes, alpha, w, h, chroma.shifts(), |p, s, _| pair_plane(p, s, layout));
            with_data(frame, PixelData::Yuv8 { planes, chroma: *chroma, alpha }, ow, oh)
        }
        PixelData::Yuv16 { planes, chroma, bits, alpha } => {
            let (planes, alpha, ow, oh) = map_yuv(planes, alpha, w, h, chroma.shifts(), |p, s, _| pair_plane(p, s, layout));
            with_data(frame, PixelData::Yuv16 { planes, chroma: *chroma, bits: *bits, alpha }, ow, oh)
        }
    }
}

/// Two frames of one format next to each other, `right` on the right (a frame-sequential pair).
/// Frames whose formats or heights differ come back as `left` alone.
pub fn join_frames(left: &VideoFrame, right: &VideoFrame) -> VideoFrame {
    let (lw, rw, h) = (left.width as usize, right.width as usize, left.height as usize);
    if left.height != right.height {
        return left.clone();
    }
    match (&left.data, &right.data) {
        (PixelData::Rgba8(a), PixelData::Rgba8(b)) => {
            let p = Plane::beside(&Plane::new(a, lw, h, 4), &Plane::new(b, rw, h, 4));
            with_data(left, PixelData::Rgba8(Arc::new(p.px)), p.w, p.h)
        }
        (PixelData::RgbaF32(a), PixelData::RgbaF32(b)) => {
            let p = Plane::beside(&Plane::new(a, lw, h, 4), &Plane::new(b, rw, h, 4));
            with_data(left, PixelData::RgbaF32(Arc::new(p.px)), p.w, p.h)
        }
        (PixelData::Yuv8 { planes: pa, chroma, alpha: aa }, PixelData::Yuv8 { planes: pb, chroma: cb, alpha: ab })
            if chroma == cb && aa.is_some() == ab.is_some() =>
        {
            let rp = planes(pb, ab, rw, h, cb.shifts());
            let (planes, alpha, ow, oh) = map_yuv(pa, aa, lw, h, chroma.shifts(), |p, _, i| Plane::beside(p, &rp[i].0));
            with_data(left, PixelData::Yuv8 { planes, chroma: *chroma, alpha }, ow, oh)
        }
        (PixelData::Yuv16 { planes: pa, chroma, bits, alpha: aa }, PixelData::Yuv16 { planes: pb, chroma: cb, bits: bb, alpha: ab })
            if chroma == cb && bits == bb && aa.is_some() == ab.is_some() =>
        {
            let rp = planes(pb, ab, rw, h, cb.shifts());
            let (planes, alpha, ow, oh) = map_yuv(pa, aa, lw, h, chroma.shifts(), |p, _, i| Plane::beside(p, &rp[i].0));
            with_data(left, PixelData::Yuv16 { planes, chroma: *chroma, bits: *bits, alpha }, ow, oh)
        }
        _ => left.clone(),
    }
}

#[cfg(test)]
#[path = "stereo_tests.rs"]
mod tests;
