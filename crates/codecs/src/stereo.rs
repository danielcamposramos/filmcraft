//! The stereo 3D layout of a video stream ([`StereoLayout`]), from the marks the bitstream and the
//! container carry.
//!
//! - In the bitstream: the H.264 (V15) 08/2024 frame packing arrangement SEI message (D.2.26)
//!   and the H.265 (V10) 07/2024 frame packing (D.3.16) and segmented rectangular frame packing
//!   (D.3.29) SEI messages of the
//!   first access unit (and HEVC's `hvcC` SEI arrays).
//! - In the container: the Spherical Video V2 `st3d` box of an MP4 / QuickTime sample entry, the
//!   Matroska `StereoMode`.
//!
//! When both carry one, FilmCraft goes by the bitstream's SEI message ([`pick`]): it travels with
//! the coded frames, so it survives remuxing. Neither specification ranks the two; that order is
//! FilmCraft's choice.

use filmcraft_media::stereo::{StereoLayout, StereoMark, StereoPacking, half_by_shape};

/// The layout an H.264 access unit's first frame packing arrangement SEI message gives (NAL units
/// without start codes or length prefixes). A message that cancels, uses field views (frames
/// FilmCraft's decoder does not produce) or reserved values gives None.
pub fn h264_layout<'a>(nals: impl IntoIterator<Item = &'a [u8]>) -> Option<StereoLayout> {
    let f = nals.into_iter().find_map(filmcraft_h264::sei::frame_packing)?;
    if f.cancel || f.field_views {
        return None;
    }
    StereoLayout::from_frame_packing(
        f.arrangement_type,
        f.content_interpretation_type,
        f.quincunx_sampling,
        f.spatial_flipping,
        f.frame0_flipped,
        f.current_frame_is_frame0,
    )
}

/// The layout an H.265 access unit's frame packing arrangement SEI message gives (Table D.8
/// defines types 3, 4 and 5; other values are ignored, D.3.16), else its segmented rectangular
/// frame packing arrangement SEI message (D.3.29). NAL units without start codes or length
/// prefixes.
pub fn hevc_layout<'a>(nals: impl IntoIterator<Item = &'a [u8]>) -> Option<StereoLayout> {
    let (mut fpa, mut seg) = (None, None);
    for nal in nals {
        let (f, s) = filmcraft_hevc::sei::frame_packing(nal);
        fpa = fpa.or(f);
        seg = seg.or(s);
    }
    if let Some(f) = fpa.filter(|f| !f.cancel && (3..=5).contains(&f.arrangement_type)) {
        return StereoLayout::from_frame_packing(
            f.arrangement_type,
            f.content_interpretation_type,
            f.quincunx_sampling,
            f.spatial_flipping,
            f.frame0_flipped,
            f.current_frame_is_frame0,
        );
    }
    let s = seg.filter(|s| !s.cancel)?;
    // Table D.12: 2 when frame 0 (region R0) is the right view
    let first = match s.content_interpretation_type {
        0 | 1 => filmcraft_media::stereo::Eye::Left,
        2 => filmcraft_media::stereo::Eye::Right,
        _ => return None,
    };
    Some(StereoLayout::new(StereoPacking::TileFormat, first, StereoMark::FramePackingSei))
}

/// FilmCraft's order: the bitstream's SEI message, else the container's mark.
pub fn pick(sei: Option<StereoLayout>, container: Option<StereoLayout>) -> Option<StereoLayout> {
    sei.or(container)
}

/// `l` with its views read as half resolution or not from the `w`×`h` frame's shape
/// ([`half_by_shape`]), for marks that don't say (SEI messages without quincunx sampling,
/// unmarked Matroska display sizes). Quincunx-sampled views stay half resolution.
pub fn by_shape(mut l: StereoLayout, w: u32, h: u32, par: (u32, u32)) -> StereoLayout {
    if !l.quincunx {
        l.half = half_by_shape(l.packing, w, h, par);
    }
    l
}

/// NAL units of a length-prefixed sample (MP4, Matroska), empty when the prefixes don't add up.
pub fn length_prefixed(sample: &[u8], length_size: usize) -> Vec<&[u8]> {
    filmcraft_bitstream::length_prefixed_nals(sample, length_size).unwrap_or_default()
}

/// The layout of an MP4 / QuickTime video track, from the frame packing SEI message of its first
/// sample `first` (H.264, H.265; also the SEI arrays of `hvcC`), else the sample entry's `st3d`
/// box. `w`×`h`, `par`: the coded frame, for [`by_shape`].
pub fn sample_entry_layout(entry: &filmcraft_isobmff::SampleEntry, first: &[u8], w: u32, h: u32, par: (u32, u32)) -> Option<StereoLayout> {
    use filmcraft_isobmff::CodecConfig;
    let sei = match &entry.codec {
        CodecConfig::Avc(c) => h264_layout(length_prefixed(first, c.length_size as usize)),
        CodecConfig::Hevc(c) => {
            let declared =
                c.arrays.iter().filter(|a| a.nal_type == filmcraft_hevc::slice::nal_type::PREFIX_SEI).flat_map(|a| a.nalus.iter().map(Vec::as_slice));
            hevc_layout(declared.chain(length_prefixed(first, c.length_size as usize)))
        }
        _ => None,
    };
    let st3d = entry.video.as_ref().and_then(|v| v.stereo_mode).and_then(StereoLayout::from_st3d);
    pick(sei.map(|l| by_shape(l, w, h, par)), st3d)
}

/// The layout of a Matroska video track and the pixel aspect ratio of its delivered views: the
/// frame packing SEI message of the first frame `first` (H.264, H.265, through the track's
/// `entry`), else the track's `StereoMode`. For the `StereoMode`, RFC 9559 §18.10 makes
/// `DisplayWidth` × `DisplayHeight` the display size of one plane: side-by-side (top-bottom) views
/// whose samples are shown at least 1.5 times as wide (high) as they are stored are read as half
/// resolution, and the pixel aspect ratio is that of their upconverted samples. Without a display
/// size the frame's shape decides ([`by_shape`]).
pub fn matroska_layout(entry: Option<&filmcraft_isobmff::SampleEntry>, v: &filmcraft_matroska::VideoInfo, first: &[u8]) -> Option<(StereoLayout, (u32, u32))> {
    let par = v.pixel_aspect();
    if let Some(l) = entry.and_then(|e| sample_entry_layout(e, first, v.pixel_width, v.pixel_height, par)) {
        return Some((l, par));
    }
    let l = StereoLayout::from_matroska(v.stereo_mode?)?;
    if !matches!(l.packing, StereoPacking::SideBySide | StereoPacking::TopBottom) {
        return Some((l, par));
    }
    if v.display_width.is_none() || v.display_height.is_none() || !matches!(v.display_unit, 0 | 3) {
        return Some((by_shape(l, v.pixel_width, v.pixel_height, (1, 1)), par));
    }
    let r = par.0 as f64 / par.1.max(1) as f64;
    let sbs = l.packing == StereoPacking::SideBySide;
    let half = if sbs { r >= 1.5 } else { r <= 1.0 / 1.5 };
    if !half {
        return Some((l, par));
    }
    let (n, d) = if sbs { (par.0 as u64, par.1 as u64 * 2) } else { (par.0 as u64 * 2, par.1 as u64) };
    let g = gcd(n, d);
    Some((StereoLayout { half, ..l }, ((n / g) as u32, (d / g) as u32)))
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use filmcraft_media::stereo::Eye;

    /// H.264 SEI NAL unit with one frame packing arrangement message (D.1.26).
    pub(crate) fn h264_fpa(arrangement_type: u8, content: u8, current_frame_is_frame0: bool) -> Vec<u8> {
        let mut p = filmcraft_bitstream::BitWriter::new();
        p.write_ue(0);
        p.write_bit(false);
        p.write_bits(arrangement_type as u32, 7);
        let quincunx = arrangement_type == 0;
        p.write_bit(quincunx);
        p.write_bits(content as u32, 6);
        p.write_bits(0, 3);
        p.write_bit(current_frame_is_frame0);
        p.write_bits(0, 2);
        if !quincunx && arrangement_type != 5 {
            p.write_bits(0, 16);
        }
        p.write_bits(0, 8);
        p.write_ue(1);
        p.write_bit(false);
        if !p.is_byte_aligned() {
            p.write_bit(true);
        }
        let payload = p.finish();
        let mut rbsp = vec![45, payload.len() as u8];
        rbsp.extend(payload);
        rbsp.push(0x80);
        let mut nal = vec![0x06];
        nal.extend(filmcraft_bitstream::escape_rbsp(&rbsp));
        nal
    }

    #[test]
    fn h264_access_units() {
        let slice = [0x65u8, 0x88, 0x80];
        let sei = h264_fpa(4, 2, false);
        let l = h264_layout([&slice[..], &sei[..]]).unwrap();
        assert_eq!((l.packing, l.first, l.mark), (StereoPacking::TopBottom, Eye::Right, StereoMark::FramePackingSei));
        // frame alternation starting with constituent frame 0, which is the left view
        let l = h264_layout([&h264_fpa(5, 1, true)[..]]).unwrap();
        assert_eq!((l.packing, l.first), (StereoPacking::FrameSequential, Eye::Left));
        assert_eq!(h264_layout([&slice[..]]), None);
        // "a complete 2D frame"
        assert_eq!(h264_layout([&h264_fpa(6, 0, false)[..]]).map(|l| l.packing), Some(StereoPacking::Mono));
    }

    #[test]
    fn container_and_bitstream_order() {
        let sei = StereoLayout::from_frame_packing(3, 2, false, false, false, false);
        let box_ = StereoLayout::from_st3d(2);
        assert_eq!(pick(sei, box_), sei);
        assert_eq!(pick(None, box_), box_);
    }

    #[test]
    fn matroska_plane_display_size() {
        let v = |w, h, d: Option<(u32, u32)>, mode| filmcraft_matroska::VideoInfo {
            pixel_width: w,
            pixel_height: h,
            display_width: d.map(|d| d.0),
            display_height: d.map(|d| d.1),
            stereo_mode: Some(mode),
            ..Default::default()
        };
        // RFC 9559 §18.10's example: 3840×1080, one plane shown 1920×1080: full-width views
        let (l, par) = matroska_layout(None, &v(3840, 1080, Some((1920, 1080)), 1), &[]).unwrap();
        assert_eq!((l.packing, l.first, l.half, par), (StereoPacking::SideBySide, Eye::Left, false, (1, 1)));
        // 1920×1080 with one plane shown 1920×1080: half-width views, square once upconverted
        let (l, par) = matroska_layout(None, &v(1920, 1080, Some((1920, 1080)), 11), &[]).unwrap();
        assert_eq!((l.first, l.half, par), (Eye::Right, true, (1, 1)));
        // a plane shown at its stored size is full resolution, as written
        let (l, _) = matroska_layout(None, &v(1920, 1080, Some((960, 1080)), 1), &[]).unwrap();
        assert!(!l.half);
        // top-bottom, half height
        let (l, par) = matroska_layout(None, &v(1920, 1080, Some((1920, 1080)), 3), &[]).unwrap();
        assert_eq!((l.packing, l.half, par), (StereoPacking::TopBottom, true, (1, 1)));
        // no display size: the frame's shape
        assert!(matroska_layout(None, &v(1920, 1080, None, 1), &[]).unwrap().0.half);
        assert!(!matroska_layout(None, &v(3840, 1080, None, 1), &[]).unwrap().0.half);
        // the bitstream's message comes first
        let entry = filmcraft_isobmff::SampleEntry::avc(filmcraft_isobmff::AvcConfig::new(vec![vec![0x67, 0x64, 0, 0x1f]], vec![vec![0x68]], 4), 1920, 1080);
        let sei = h264_fpa(4, 1, false);
        let mut sample = (sei.len() as u32).to_be_bytes().to_vec();
        sample.extend(&sei);
        let (l, _) = matroska_layout(Some(&entry), &v(1920, 1080, None, 1), &sample).unwrap();
        assert_eq!((l.packing, l.mark), (StereoPacking::TopBottom, StereoMark::FramePackingSei));
        // no mark at all
        let mono = filmcraft_matroska::VideoInfo { pixel_width: 16, pixel_height: 16, ..Default::default() };
        assert_eq!(matroska_layout(Some(&entry), &mono, &[]), None);
    }

    #[test]
    fn shape_decides_half_resolution() {
        let l = StereoLayout::from_frame_packing(3, 1, false, false, false, false).unwrap();
        assert!(by_shape(l, 1920, 1080, (1, 1)).half);
        assert!(!by_shape(l, 3840, 1080, (1, 1)).half);
        let q = StereoLayout::from_frame_packing(3, 1, true, false, false, false).unwrap();
        assert!(by_shape(q, 3840, 1080, (1, 1)).half, "quincunx views are always upconverted");
    }
}
