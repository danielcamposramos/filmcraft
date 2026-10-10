//! Supplemental enhancement information (7.3.5, Annex D) that FilmCraft reads outside the
//! decoding process (the decoder itself skips SEI NAL units). Clause and table numbers are those
//! of Rec. ITU-T H.265 (V10) 07/2024.
//!
//! - [`messages`]: the `sei_message()`s of an SEI RBSP (7.3.5) as payload type and payload.
//! - [`FramePacking`]: the frame packing arrangement message (syntax D.2.16, semantics D.3.16).
//! - [`SegmentedRectFramePacking`]: the segmented rectangular frame packing arrangement message
//!   (syntax D.2.29, semantics D.3.29), the tile packing of a 720p pair in a 1080p frame.
//! - [`frame_packing`]: the stereo packing messages of a prefix SEI NAL unit.

use filmcraft_bitstream::{BitReader, unescape_rbsp};

use crate::error::Result;
use crate::slice::nal_type;

/// `payloadType` of the frame packing arrangement SEI message (D.2.1).
pub const FRAME_PACKING_ARRANGEMENT: u32 = 45;
/// `payloadType` of the segmented rectangular frame packing arrangement SEI message (D.2.1).
pub const SEGMENTED_RECT_FRAME_PACKING_ARRANGEMENT: u32 = 138;

/// A `payloadType` or `payloadSize`: 0xFF bytes adding 255 each, then the last byte (7.3.5).
fn ff_coded(rbsp: &[u8], i: &mut usize) -> Option<u32> {
    let mut v = 0u32;
    loop {
        let b = *rbsp.get(*i)?;
        *i += 1;
        v = v.checked_add(b as u32)?;
        if b != 0xFF {
            return Some(v);
        }
    }
}

/// The `sei_message()`s of an SEI RBSP (emulation prevention bytes already removed), as
/// (payloadType, payload) in bitstream order. The list ends at the `rbsp_trailing_bits()`, or
/// early at a message whose header or payload runs past the end of the data.
pub fn messages(rbsp: &[u8]) -> Vec<(u32, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    // more_rbsp_data(): what remains is more than the trailing bits (0x80 then zero bytes)
    while i < rbsp.len() && !(rbsp[i] == 0x80 && rbsp[i + 1..].iter().all(|&b| b == 0)) {
        let (Some(t), Some(n)) = (ff_coded(rbsp, &mut i), ff_coded(rbsp, &mut i)) else { break };
        let Some(payload) = rbsp.get(i..i + n as usize) else { break };
        i += n as usize;
        out.push((t, payload));
    }
    out
}

/// A frame packing arrangement SEI message (D.2.16). Field meanings are those of D.3.16.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FramePacking {
    /// `frame_packing_arrangement_id`.
    pub id: u32,
    /// `frame_packing_arrangement_cancel_flag`: the message ends the persistence of an earlier one
    /// and carries no arrangement.
    pub cancel: bool,
    /// `frame_packing_arrangement_type` (Table D.8): 3 side by side, 4 top-bottom, 5 temporal
    /// interleaving of alternating frames. Other values are reserved and decoders ignore the
    /// message.
    pub arrangement_type: u8,
    /// `quincunx_sampling_flag`: each constituent frame is quincunx sampled (Figure D.8).
    pub quincunx_sampling: bool,
    /// `content_interpretation_type` (Table D.9): 0 unspecified relationship, 1 frame 0 is the
    /// left view and frame 1 the right view, 2 frame 0 is the right view and frame 1 the left.
    pub content_interpretation_type: u8,
    /// `spatial_flipping_flag`: one constituent frame is stored mirrored (horizontally for side
    /// by side, vertically for top-bottom).
    pub spatial_flipping: bool,
    /// `frame0_flipped_flag`: the mirrored frame is frame 0 (else frame 1).
    pub frame0_flipped: bool,
    /// `field_views_flag` (reserved: 0 in conforming streams).
    pub field_views: bool,
    /// `current_frame_is_frame0_flag`: with type 5, this frame is frame 0 (else frame 1).
    pub current_frame_is_frame0: bool,
    pub frame0_self_contained: bool,
    pub frame1_self_contained: bool,
    /// `frame0_grid_position_x`, `frame0_grid_position_y`, `frame1_grid_position_x`,
    /// `frame1_grid_position_y` (in 1/16 luma samples), present without quincunx sampling and
    /// for every type but 5.
    pub grid_positions: Option<[u8; 4]>,
    /// `frame_packing_arrangement_persistence_flag`: the message persists for the layer in output
    /// order (else it applies to the current frame only).
    pub persistence: bool,
    /// `upsampled_aspect_ratio_flag`: the SPS VUI sample aspect ratio is that of the samples
    /// after upconversion of each constituent frame (else before it).
    pub upsampled_aspect_ratio: bool,
}

impl FramePacking {
    /// Parse a frame packing arrangement payload (payloadType 45).
    pub fn parse(payload: &[u8]) -> Result<Self> {
        let mut r = BitReader::new(payload);
        let mut f = FramePacking { id: r.read_ue()?, cancel: r.read_flag()?, ..Default::default() };
        if !f.cancel {
            f.arrangement_type = r.read_bits(7)? as u8;
            f.quincunx_sampling = r.read_flag()?;
            f.content_interpretation_type = r.read_bits(6)? as u8;
            f.spatial_flipping = r.read_flag()?;
            f.frame0_flipped = r.read_flag()?;
            f.field_views = r.read_flag()?;
            f.current_frame_is_frame0 = r.read_flag()?;
            f.frame0_self_contained = r.read_flag()?;
            f.frame1_self_contained = r.read_flag()?;
            if !f.quincunx_sampling && f.arrangement_type != 5 {
                let mut g = [0u8; 4];
                for v in &mut g {
                    *v = r.read_bits(4)? as u8;
                }
                f.grid_positions = Some(g);
            }
            let _reserved_byte = r.read_bits(8)?;
            f.persistence = r.read_flag()?;
        }
        f.upsampled_aspect_ratio = r.read_flag()?;
        Ok(f)
    }
}

/// A segmented rectangular frame packing arrangement SEI message (D.2.29). Constituent frame 0
/// is the region R0 (the top-left two thirds by two thirds of the cropped frame); frame 1 is R1
/// (right of R0) with R2 above R3 (the first two thirds of the bottom third) placed to its right
/// (D.3.29, Figure D.10).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SegmentedRectFramePacking {
    /// `segmented_rect_frame_packing_arrangement_cancel_flag`.
    pub cancel: bool,
    /// `segmented_rect_content_interpretation_type` (Table D.12): 0 unspecified relationship,
    /// 1 frame 0 is the left view, 2 frame 0 is the right view.
    pub content_interpretation_type: u8,
    /// `segmented_rect_frame_packing_arrangement_persistence_flag`.
    pub persistence: bool,
}

impl SegmentedRectFramePacking {
    /// Parse a segmented rectangular frame packing arrangement payload (payloadType 138).
    pub fn parse(payload: &[u8]) -> Result<Self> {
        let mut r = BitReader::new(payload);
        let mut f = SegmentedRectFramePacking { cancel: r.read_flag()?, ..Default::default() };
        if !f.cancel {
            f.content_interpretation_type = r.read_bits(2)? as u8;
            f.persistence = r.read_flag()?;
        }
        Ok(f)
    }
}

/// The stereo packing messages of one NAL unit (2-byte header included): the first frame packing
/// arrangement message and the first segmented rectangular one that parse. Both are prefix SEI
/// messages (Table D.1); other NAL units and other layers than the base layer give nothing.
pub fn frame_packing(nal: &[u8]) -> (Option<FramePacking>, Option<SegmentedRectFramePacking>) {
    let [h0, h1, body @ ..] = nal else { return (None, None) };
    let (kind, layer_id) = ((h0 >> 1) & 0x3f, ((h0 & 1) << 5) | (h1 >> 3));
    if kind != nal_type::PREFIX_SEI || layer_id != 0 {
        return (None, None);
    }
    let rbsp = unescape_rbsp(body);
    let ms = messages(&rbsp);
    let fpa = ms.iter().filter(|(t, _)| *t == FRAME_PACKING_ARRANGEMENT).find_map(|(_, p)| FramePacking::parse(p).ok());
    let srfpa = ms.iter().filter(|(t, _)| *t == SEGMENTED_RECT_FRAME_PACKING_ARRANGEMENT).find_map(|(_, p)| SegmentedRectFramePacking::parse(p).ok());
    (fpa, srfpa)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A prefix SEI NAL unit written out bit by bit from D.2.16 (not by a writer under test):
    /// id 0, top-bottom (type 4), frame 0 is the left view (content interpretation 1), grid
    /// positions (0, 0) (0, 0), persistence 1, upsampled_aspect_ratio_flag 0. Payload bits:
    /// `1 0 0000100 0 000001 000000 0…0 (16) 00000000 1 0` (48 bits, so no alignment bits);
    /// 0x00 0x00 0x00 needs an emulation prevention byte.
    const TB_LEFT_FIRST: [u8; 12] = [0x4E, 0x01, 0x2D, 0x06, 0x82, 0x01, 0x00, 0x00, 0x03, 0x00, 0x02, 0x80];

    #[test]
    fn top_bottom_message_from_the_syntax_table() {
        let (f, s) = frame_packing(&TB_LEFT_FIRST);
        assert_eq!(s, None);
        assert_eq!(
            f,
            Some(FramePacking { arrangement_type: 4, content_interpretation_type: 1, grid_positions: Some([0; 4]), persistence: true, ..Default::default() })
        );
    }

    fn payload(f: &FramePacking) -> Vec<u8> {
        let mut w = filmcraft_bitstream::BitWriter::new();
        w.write_ue(f.id);
        w.write_bit(f.cancel);
        if !f.cancel {
            w.write_bits(f.arrangement_type as u32, 7);
            w.write_bit(f.quincunx_sampling);
            w.write_bits(f.content_interpretation_type as u32, 6);
            for b in [f.spatial_flipping, f.frame0_flipped, f.field_views, f.current_frame_is_frame0, f.frame0_self_contained, f.frame1_self_contained] {
                w.write_bit(b);
            }
            if let Some(g) = f.grid_positions {
                for v in g {
                    w.write_bits(v as u32, 4);
                }
            }
            w.write_bits(0, 8);
            w.write_bit(f.persistence);
        }
        w.write_bit(f.upsampled_aspect_ratio);
        if !w.is_byte_aligned() {
            w.write_bit(true);
        }
        w.finish()
    }

    fn sei_nal(kind: u8, messages: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let mut rbsp = Vec::new();
        for (t, p) in messages {
            let (mut t, mut n) = (*t, p.len());
            while t >= 255 {
                rbsp.push(0xFF);
                t -= 255;
            }
            rbsp.push(t as u8);
            while n >= 255 {
                rbsp.push(0xFF);
                n -= 255;
            }
            rbsp.push(n as u8);
            rbsp.extend_from_slice(p);
        }
        rbsp.push(0x80);
        let mut nal = vec![kind << 1, 0x01];
        nal.extend(filmcraft_bitstream::escape_rbsp(&rbsp));
        nal
    }

    #[test]
    fn every_arrangement_type_and_both_view_orders() {
        for arrangement_type in 3..=5u8 {
            for content_interpretation_type in 0..=2u8 {
                for quincunx_sampling in [false, arrangement_type != 5] {
                    let f = FramePacking {
                        id: 2,
                        arrangement_type,
                        quincunx_sampling,
                        content_interpretation_type,
                        current_frame_is_frame0: arrangement_type == 5,
                        grid_positions: (!quincunx_sampling && arrangement_type != 5).then_some([4, 8, 4, 8]),
                        persistence: true,
                        upsampled_aspect_ratio: true,
                        ..Default::default()
                    };
                    let nal = sei_nal(nal_type::PREFIX_SEI, &[(FRAME_PACKING_ARRANGEMENT, payload(&f))]);
                    assert_eq!(frame_packing(&nal).0, Some(f), "type {arrangement_type}, content {content_interpretation_type}");
                }
            }
        }
    }

    #[test]
    fn segmented_rectangular_packing() {
        for content in 0..=2u8 {
            // cancel 0, content (2 bits), persistence 1, then the alignment bits 1 000
            let p = vec![(content << 5) | 0x10 | 0x08];
            let nal = sei_nal(nal_type::PREFIX_SEI, &[(5, vec![0; 18]), (SEGMENTED_RECT_FRAME_PACKING_ARRANGEMENT, p)]);
            assert_eq!(frame_packing(&nal), (None, Some(SegmentedRectFramePacking { cancel: false, content_interpretation_type: content, persistence: true })));
        }
        let cancel = sei_nal(nal_type::PREFIX_SEI, &[(SEGMENTED_RECT_FRAME_PACKING_ARRANGEMENT, vec![0xC0])]);
        assert_eq!(frame_packing(&cancel).1, Some(SegmentedRectFramePacking { cancel: true, ..Default::default() }));
    }

    /// Truncations and single-bit flips of valid SEI NAL units never panic (AGENTS.md §0.8).
    #[test]
    fn mutations_never_panic() {
        let f = FramePacking { arrangement_type: 5, content_interpretation_type: 2, current_frame_is_frame0: true, persistence: true, ..Default::default() };
        let multi = sei_nal(
            nal_type::PREFIX_SEI,
            &[(5, vec![0x22; 40]), (FRAME_PACKING_ARRANGEMENT, payload(&f)), (SEGMENTED_RECT_FRAME_PACKING_ARRANGEMENT, vec![0x38])],
        );
        for nal in [TB_LEFT_FIRST.to_vec(), multi] {
            for n in 0..=nal.len() {
                assert!(std::panic::catch_unwind(|| frame_packing(&nal[..n])).is_ok(), "truncated to {n}");
            }
            for bit in 0..nal.len() * 8 {
                let mut m = nal.clone();
                m[bit / 8] ^= 0x80 >> (bit % 8);
                assert!(std::panic::catch_unwind(|| frame_packing(&m)).is_ok(), "bit {bit} flipped");
            }
        }
        assert!(std::panic::catch_unwind(|| messages(&[0xFF; 600])).is_ok());
    }

    #[test]
    fn suffix_sei_other_layers_and_damaged_messages_give_nothing() {
        let f = FramePacking { arrangement_type: 3, content_interpretation_type: 1, grid_positions: Some([0; 4]), persistence: true, ..Default::default() };
        assert_eq!(frame_packing(&sei_nal(nal_type::SUFFIX_SEI, &[(FRAME_PACKING_ARRANGEMENT, payload(&f))])), (None, None));
        let mut layer1 = sei_nal(nal_type::PREFIX_SEI, &[(FRAME_PACKING_ARRANGEMENT, payload(&f))]);
        layer1[1] |= 1 << 3;
        assert_eq!(frame_packing(&layer1), (None, None));
        assert_eq!(frame_packing(&[0x4E, 0x01, 0x2D, 0x40, 0x81, 0x80]), (None, None));
        assert_eq!(frame_packing(&[0x4E]), (None, None));
    }
}
