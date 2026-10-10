//! Supplemental enhancement information (7.3.2.3, Annex D) that FilmCraft reads outside the
//! decoding process (the decoder itself skips SEI NAL units). Clause and table numbers are those
//! of Rec. ITU-T H.264 (V15) 08/2024; other editions number Annex D differently (the 2012 edition has the frame packing
//! arrangement message at D.1.25 and D.2.25).
//!
//! - [`messages`]: the `sei_message()`s of an SEI RBSP (7.3.2.3.1) as payload type and payload.
//! - [`FramePacking`]: the frame packing arrangement message (syntax D.1.26, semantics D.2.26),
//!   which says how the two views of a stereo pair are packed into the decoded frames.
//! - [`frame_packing`]: the first frame packing arrangement message of an SEI NAL unit.

use filmcraft_bitstream::{BitReader, unescape_rbsp};

use crate::error::Result;
use crate::slice::nal_type;

/// `payloadType` of the frame packing arrangement SEI message (D.1.1).
pub const FRAME_PACKING_ARRANGEMENT: u32 = 45;

/// A `payloadType` or `payloadSize`: 0xFF bytes adding 255 each, then the last byte (7.3.2.3.1).
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

/// A frame packing arrangement SEI message (D.1.26). Field meanings are those of D.2.26.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FramePacking {
    /// `frame_packing_arrangement_id`.
    pub id: u32,
    /// `frame_packing_arrangement_cancel_flag`: the message ends the persistence of an earlier one
    /// and carries no arrangement (the other fields are zero).
    pub cancel: bool,
    /// `frame_packing_arrangement_type` (Table D-9): 0 checkerboard interleaving, 1 column
    /// interleaving, 2 row interleaving, 3 side by side, 4 top-bottom, 5 temporal interleaving of
    /// alternating frames, 6 a complete 2D frame without packing, 7 tile format. Other values are
    /// reserved.
    pub arrangement_type: u8,
    /// `quincunx_sampling_flag`: each constituent frame is quincunx sampled (Figure D-1, D-10).
    pub quincunx_sampling: bool,
    /// `content_interpretation_type` (Table D-10): 0 unspecified relationship, 1 frame 0 is the
    /// left view and frame 1 the right view, 2 frame 0 is the right view and frame 1 the left.
    pub content_interpretation_type: u8,
    /// `spatial_flipping_flag`: with types 3 and 4, one constituent frame is stored mirrored
    /// (horizontally for side by side, vertically for top-bottom).
    pub spatial_flipping: bool,
    /// `frame0_flipped_flag`: the mirrored frame is frame 0 (else frame 1).
    pub frame0_flipped: bool,
    /// `field_views_flag`: the fields of one parity are frame 0, the other parity frame 1.
    pub field_views: bool,
    /// `current_frame_is_frame0_flag`: with type 5, this frame is frame 0 (else frame 1).
    pub current_frame_is_frame0: bool,
    pub frame0_self_contained: bool,
    pub frame1_self_contained: bool,
    /// `frame0_grid_position_x`, `frame0_grid_position_y`, `frame1_grid_position_x`,
    /// `frame1_grid_position_y` (in 1/16 luma samples), present without quincunx sampling and
    /// for every type but 5.
    pub grid_positions: Option<[u8; 4]>,
    /// `frame_packing_arrangement_repetition_period`: 0 for the current frame only, 1 until
    /// a new coded video sequence or a newer message, more as D.2.26 says.
    pub repetition_period: u32,
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
            f.repetition_period = r.read_ue()?;
        }
        // frame_packing_arrangement_extension_flag (0 in conforming streams) follows
        Ok(f)
    }
}

/// The first frame packing arrangement message of a NAL unit (header byte included), if it is an
/// SEI NAL unit that carries one that parses.
pub fn frame_packing(nal: &[u8]) -> Option<FramePacking> {
    let (&header, body) = nal.split_first()?;
    if header & 0x1f != nal_type::SEI {
        return None;
    }
    let rbsp = unescape_rbsp(body);
    messages(&rbsp).into_iter().find(|(t, _)| *t == FRAME_PACKING_ARRANGEMENT).and_then(|(_, p)| FramePacking::parse(p).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An SEI NAL unit written out bit by bit from D.1.26 (not by a writer under test): id 0,
    /// side by side (type 3), frame 0 is the left view (content interpretation 1), grid positions
    /// (0, 0) (0, 0), repetition period 1. Payload bits: `1 0 0000011 0 000001 000000 0…0 (16)
    /// 00000000 010 0`, then the payload's alignment bits `1 00000`; 0x00 0x00 0x00 needs an
    /// emulation prevention byte.
    const SBS_LEFT_FIRST: [u8; 12] = [0x06, 0x2D, 0x07, 0x81, 0x81, 0x00, 0x00, 0x03, 0x00, 0x01, 0x20, 0x80];

    #[test]
    fn side_by_side_message_from_the_syntax_table() {
        let f = frame_packing(&SBS_LEFT_FIRST).expect("frame packing");
        assert_eq!(
            f,
            FramePacking {
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
                grid_positions: Some([0; 4]),
                repetition_period: 1,
            }
        );
    }

    /// Writes a message with `filmcraft_bitstream::BitWriter` from the D.1.26 syntax.
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
            w.write_ue(f.repetition_period);
        }
        w.write_bit(false);
        if !w.is_byte_aligned() {
            w.write_bit(true);
        }
        w.finish()
    }

    fn sei_nal(messages: &[(u32, Vec<u8>)]) -> Vec<u8> {
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
        let mut nal = vec![0x06];
        nal.extend(filmcraft_bitstream::escape_rbsp(&rbsp));
        nal
    }

    #[test]
    fn every_arrangement_type_and_both_view_orders() {
        for arrangement_type in 0..=7u8 {
            for content_interpretation_type in 0..=2u8 {
                let quincunx_sampling = arrangement_type == 0;
                let f = FramePacking {
                    id: 7,
                    arrangement_type,
                    quincunx_sampling,
                    content_interpretation_type,
                    current_frame_is_frame0: arrangement_type == 5,
                    grid_positions: (!quincunx_sampling && arrangement_type != 5).then_some([4, 8, 4, 8]),
                    repetition_period: 1,
                    ..Default::default()
                };
                let nal = sei_nal(&[(FRAME_PACKING_ARRANGEMENT, payload(&f))]);
                assert_eq!(frame_packing(&nal), Some(f), "type {arrangement_type}, content {content_interpretation_type}");
            }
        }
    }

    #[test]
    fn flipping_and_cancel() {
        let f = FramePacking {
            arrangement_type: 4,
            content_interpretation_type: 2,
            spatial_flipping: true,
            frame0_flipped: true,
            grid_positions: Some([0; 4]),
            ..Default::default()
        };
        assert_eq!(frame_packing(&sei_nal(&[(FRAME_PACKING_ARRANGEMENT, payload(&f))])), Some(f));
        let c = FramePacking { id: 3, cancel: true, ..Default::default() };
        assert_eq!(frame_packing(&sei_nal(&[(FRAME_PACKING_ARRANGEMENT, payload(&c))])), Some(c));
    }

    #[test]
    fn found_after_other_messages_and_long_headers() {
        // a user data message of 300 bytes (two-byte size) and a type above 255 come first
        let f = FramePacking { arrangement_type: 3, content_interpretation_type: 1, grid_positions: Some([0; 4]), repetition_period: 1, ..Default::default() };
        let nal = sei_nal(&[(5, vec![0x11; 300]), (300, vec![1, 2, 3]), (FRAME_PACKING_ARRANGEMENT, payload(&f))]);
        let rbsp = unescape_rbsp(&nal[1..]);
        let ms = messages(&rbsp);
        assert_eq!(ms.iter().map(|(t, p)| (*t, p.len())).collect::<Vec<_>>(), vec![(5, 300), (300, 3), (45, payload(&f).len())]);
        assert_eq!(frame_packing(&nal), Some(f));
    }

    /// Truncations and single-bit flips of valid SEI NAL units never panic (AGENTS.md §0.8).
    #[test]
    fn mutations_never_panic() {
        let f = FramePacking { arrangement_type: 3, content_interpretation_type: 1, grid_positions: Some([0; 4]), repetition_period: 1, ..Default::default() };
        let multi = sei_nal(&[(5, vec![0x11; 40]), (FRAME_PACKING_ARRANGEMENT, payload(&f))]);
        for nal in [SBS_LEFT_FIRST.to_vec(), multi] {
            for n in 0..=nal.len() {
                assert!(std::panic::catch_unwind(|| frame_packing(&nal[..n])).is_ok(), "truncated to {n}");
            }
            for bit in 0..nal.len() * 8 {
                let mut m = nal.clone();
                m[bit / 8] ^= 0x80 >> (bit % 8);
                assert!(std::panic::catch_unwind(|| frame_packing(&m)).is_ok(), "bit {bit} flipped");
            }
        }
        // sizes and types made of 0xFF bytes
        assert!(std::panic::catch_unwind(|| messages(&[0xFF; 600])).is_ok());
    }

    #[test]
    fn other_nal_units_and_damaged_messages_give_nothing() {
        // not an SEI NAL unit (a PPS header byte)
        let mut pps = SBS_LEFT_FIRST;
        pps[0] = 0x68;
        assert_eq!(frame_packing(&pps), None);
        // no frame packing message
        assert_eq!(frame_packing(&sei_nal(&[(5, vec![0; 20])])), None);
        // a payload size past the end
        assert_eq!(frame_packing(&[0x06, 0x2D, 0x40, 0x81, 0x80]), None);
        assert_eq!(frame_packing(&[]), None);
    }
}
