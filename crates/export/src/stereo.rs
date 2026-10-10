//! The stereo mark of exports. A stereo sequence ([`filmcraft_project::SequenceSettings::stereo`])
//! renders full side by side with the left eye first, and its exports say so:
//!
//! - FilmCraft's H.264 encoder sends a frame packing arrangement SEI message (side by side,
//!   constituent frame 0 is the left view: Rec. ITU-T H.264 (V15) 08/2024 D.2.26, Tables D-9 and D-10) with every access
//!   unit;
//! - the H.264 and H.265 streams of other encoders (the platforms' hardware encoders) get the same
//!   message (Rec. ITU-T H.265 (V10) 07/2024 D.3.16) inserted before the first slice of every access unit
//!   ([`with_frame_packing`]);
//! - MP4 and QuickTime video sample entries get a Spherical Video V2 `st3d` box with
//!   `stereo_mode` 2 (left-right), whatever the codec.

use filmcraft_bitstream::{BitWriter, escape_rbsp, unescape_rbsp};

/// `st3d` `stereo_mode` of FilmCraft's stereo exports: left-right.
pub const ST3D_LEFT_RIGHT: u8 = 2;

/// How the NAL units of an access unit are delimited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// A big-endian length of this many bytes before each NAL unit (MP4 samples).
    LengthPrefixed(usize),
    /// Start codes (Annex B byte stream: MXF).
    AnnexB,
}

/// The H.264 SEI NAL unit of FilmCraft's stereo exports.
pub fn h264_sei() -> Vec<u8> {
    filmcraft_h264enc::FramePacking::SIDE_BY_SIDE_LEFT_FIRST.sei_nal()
}

/// The H.265 prefix SEI NAL unit of FilmCraft's stereo exports (D.2.16): id 0, side by side (3),
/// no quincunx sampling, frame 0 is the left view (1), no flipping, frames not self-contained, the
/// type's default sampling grid, persistence 1, `upsampled_aspect_ratio_flag` 0 (the VUI sample
/// aspect ratio is that of the views as stored, which are full resolution).
pub fn hevc_sei() -> Vec<u8> {
    let mut p = BitWriter::new();
    p.write_ue(0); // frame_packing_arrangement_id
    p.write_bit(false); // frame_packing_arrangement_cancel_flag
    p.write_bits(3, 7); // frame_packing_arrangement_type
    p.write_bit(false); // quincunx_sampling_flag
    p.write_bits(1, 6); // content_interpretation_type
    p.write_bits(0, 6); // flipping, field views, current frame, self-contained flags
    p.write_bits(0, 16); // frame0/1_grid_position_x/y
    p.write_bits(0, 8); // frame_packing_arrangement_reserved_byte
    p.write_bit(true); // frame_packing_arrangement_persistence_flag
    p.write_bit(false); // upsampled_aspect_ratio_flag
    let payload = p.finish();
    let mut rbsp = vec![45, payload.len() as u8];
    rbsp.extend(payload);
    rbsp.push(0x80);
    // nal_unit_type 39 (PREFIX_SEI_NUT), nuh_layer_id 0, nuh_temporal_id_plus1 1
    let mut nal = vec![39 << 1, 1];
    nal.extend(escape_rbsp(&rbsp));
    nal
}

fn nal_type(nal: &[u8], hevc: bool) -> Option<u8> {
    nal.first().map(|b| if hevc { (b >> 1) & 0x3f } else { b & 0x1f })
}

fn is_vcl(t: u8, hevc: bool) -> bool {
    if hevc { t < 32 } else { matches!(t, 1..=5 | 20 | 21) }
}

/// Whether an SEI NAL unit holds a frame packing arrangement message (payloadType 45).
fn has_frame_packing(nal: &[u8], hevc: bool) -> bool {
    let header = if hevc { 2 } else { 1 };
    if nal_type(nal, hevc) != Some(if hevc { 39 } else { 6 }) || nal.len() <= header {
        return false;
    }
    let rbsp = unescape_rbsp(&nal[header..]);
    let mut i = 0;
    let next = |i: &mut usize| -> Option<u32> {
        let mut v = 0u32;
        loop {
            let b = *rbsp.get(*i)?;
            *i += 1;
            v = v.checked_add(b as u32)?;
            if b != 0xFF {
                return Some(v);
            }
        }
    };
    while i < rbsp.len() && rbsp[i] != 0x80 {
        let (Some(t), Some(n)) = (next(&mut i), next(&mut i)) else { return false };
        if t == 45 {
            return true;
        }
        i += n as usize;
    }
    false
}

/// The NAL units of an access unit (without their prefixes or start codes).
fn nal_units(au: &[u8], framing: Framing) -> Option<Vec<&[u8]>> {
    match framing {
        Framing::LengthPrefixed(n) => filmcraft_bitstream::length_prefixed_nals(au, n).ok(),
        Framing::AnnexB => Some(filmcraft_bitstream::annexb_nals(au)),
    }
}

/// An H.264 (`hevc` false) or H.265 access unit with `sei` (an SEI NAL unit) before its first
/// slice. Access units that already carry a frame packing arrangement message, have no slice, or
/// can't be split into NAL units come back unchanged.
pub fn with_frame_packing(au: &[u8], hevc: bool, framing: Framing, sei: &[u8]) -> Vec<u8> {
    let Some(nals) = nal_units(au, framing) else { return au.to_vec() };
    if nals.iter().any(|n| has_frame_packing(n, hevc)) || !nals.iter().any(|n| nal_type(n, hevc).is_some_and(|t| is_vcl(t, hevc))) {
        return au.to_vec();
    }
    let put = |out: &mut Vec<u8>, nal: &[u8]| match framing {
        Framing::LengthPrefixed(n) => {
            out.extend_from_slice(&(nal.len() as u64).to_be_bytes()[8 - n.min(8)..]);
            out.extend_from_slice(nal);
        }
        Framing::AnnexB => {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
        }
    };
    let mut out = Vec::with_capacity(au.len() + sei.len() + 8);
    let mut done = false;
    for nal in nals {
        if !done && nal_type(nal, hevc).is_some_and(|t| is_vcl(t, hevc)) {
            put(&mut out, sei);
            done = true;
        }
        put(&mut out, nal);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefixed(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter().flat_map(|n| (n.len() as u32).to_be_bytes().into_iter().chain(n.iter().copied())).collect()
    }

    #[test]
    fn h264_message_goes_before_the_first_slice() {
        let (aud, sps, sei5, idr, slice) = ([0x09u8, 0x10], [0x67u8, 0x64, 0, 0x1f], [0x06u8, 5, 1, 0, 0x80], [0x65u8, 0x88, 0x84], [0x01u8, 0x9a, 0x02]);
        let au = prefixed(&[&aud, &sps, &sei5, &idr, &slice]);
        let out = with_frame_packing(&au, false, Framing::LengthPrefixed(4), &h264_sei());
        let nals = filmcraft_bitstream::length_prefixed_nals(&out, 4).unwrap();
        assert_eq!(nals.len(), 6);
        assert_eq!((nals[0], nals[1], nals[2], nals[4], nals[5]), (&aud[..], &sps[..], &sei5[..], &idr[..], &slice[..]));
        let f = filmcraft_h264::sei::frame_packing(nals[3]).expect("frame packing");
        assert_eq!((f.arrangement_type, f.content_interpretation_type, f.repetition_period), (3, 1, 1));
        // once is enough: an access unit that has one is left alone
        assert_eq!(with_frame_packing(&out, false, Framing::LengthPrefixed(4), &h264_sei()), out);
        // a byte stream (MXF)
        let annexb: Vec<u8> = [&aud[..], &idr[..]].iter().flat_map(|n| [0, 0, 0, 1].into_iter().chain(n.iter().copied())).collect();
        let out = with_frame_packing(&annexb, false, Framing::AnnexB, &h264_sei());
        let nals = filmcraft_bitstream::annexb_nals(&out);
        assert_eq!(nals.len(), 3);
        assert!(filmcraft_h264::sei::frame_packing(nals[1]).is_some());
        // two-byte lengths
        let au = [0u8, 3, 0x65, 0x88, 0x84];
        let out = with_frame_packing(&au, false, Framing::LengthPrefixed(2), &h264_sei());
        assert_eq!(filmcraft_bitstream::length_prefixed_nals(&out, 2).unwrap().len(), 2);
    }

    #[test]
    fn hevc_message_goes_before_the_first_slice() {
        let (vps, idr) = ([0x40u8, 0x01, 0x0c], [0x26u8, 0x01, 0xaf, 0x08]);
        let au = prefixed(&[&vps, &idr]);
        let out = with_frame_packing(&au, true, Framing::LengthPrefixed(4), &hevc_sei());
        let nals = filmcraft_bitstream::length_prefixed_nals(&out, 4).unwrap();
        assert_eq!(nals.len(), 3);
        let (f, _) = filmcraft_hevc::sei::frame_packing(nals[1]);
        let f = f.expect("frame packing");
        assert_eq!((f.arrangement_type, f.content_interpretation_type, f.persistence, f.upsampled_aspect_ratio), (3, 1, true, false));
        assert_eq!(with_frame_packing(&out, true, Framing::LengthPrefixed(4), &hevc_sei()), out);
    }

    /// Truncated and bit-flipped access units never panic (AGENTS.md §0.8).
    #[test]
    fn mutations_never_panic() {
        let au = prefixed(&[&[0x09, 0x10], &[0x06, 5, 1, 0, 0x80], &[0x65, 0x88, 0x84, 0x21]]);
        for (hevc, framing) in
            [(false, Framing::LengthPrefixed(4)), (true, Framing::LengthPrefixed(4)), (false, Framing::AnnexB), (true, Framing::LengthPrefixed(2))]
        {
            let sei = if hevc { hevc_sei() } else { h264_sei() };
            for n in 0..=au.len() {
                assert!(std::panic::catch_unwind(|| with_frame_packing(&au[..n], hevc, framing, &sei)).is_ok(), "truncated to {n}");
            }
            for bit in 0..au.len() * 8 {
                let mut m = au.clone();
                m[bit / 8] ^= 0x80 >> (bit % 8);
                assert!(std::panic::catch_unwind(|| with_frame_packing(&m, hevc, framing, &sei)).is_ok(), "bit {bit} flipped");
            }
        }
        // an SEI NAL unit whose payload sizes are all 0xFF
        let ff = prefixed(&[&[0x06, 0xFF, 0xFF, 0xFF, 0xFF], &[0x65, 0x88]]);
        assert!(std::panic::catch_unwind(|| with_frame_packing(&ff, false, Framing::LengthPrefixed(4), &h264_sei())).is_ok());
    }

    #[test]
    fn units_without_slices_or_framing_are_left_alone() {
        // parameter sets only
        let au = prefixed(&[&[0x67, 0x64, 0, 0x1f]]);
        assert_eq!(with_frame_packing(&au, false, Framing::LengthPrefixed(4), &h264_sei()), au);
        // lengths that don't add up
        let bad = [0u8, 0, 0, 9, 0x65];
        assert_eq!(with_frame_packing(&bad, false, Framing::LengthPrefixed(4), &h264_sei()), bad.to_vec());
    }
}
