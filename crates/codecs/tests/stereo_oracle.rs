//! Stereo layouts read from files other tools wrote: H.264 frame packing SEI messages from libx264
//! (every arrangement type of Table D-9 of H.264 (V15) 08/2024) in MP4, Matroska and MPEG-TS; H.265 frame packing
//! and segmented rectangular frame packing SEI messages (written into a libx265 stream, which
//! ffprobe reads back as stereo); every Matroska `StereoMode` FFmpeg writes; and the `st3d` box
//! FFmpeg writes into MP4. ffmpeg and ffprobe are fixture generators and oracles only.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::*;
use filmcraft_media::MediaInfo;
use filmcraft_media::stereo::StereoPacking::*;
use filmcraft_media::stereo::{Eye, StereoLayout, StereoMark, StereoPacking};

fn info(p: &Path) -> MediaInfo {
    let name = p.file_name().unwrap().to_string_lossy().to_string();
    filmcraft_media::open_bytes(&name, bytes(p), &filmcraft_codecs::openers()).unwrap_or_else(|e| panic!("{name}: {e}")).info().clone()
}

fn stereo(p: &Path) -> Option<StereoLayout> {
    info(p).video.and_then(|v| v.stereo)
}

const V64X32: [&str; 6] = ["-f", "lavfi", "-i", "testsrc2=size=64x32:rate=25", "-frames:v", "4"];

#[test]
fn libx264_frame_packing_every_type() {
    let ff = filmcraft_testkit::require_ffmpeg!();
    // Table D-9; libx264 writes content_interpretation_type 1 (frame 0 is the left view) and starts
    // frame alternation with frame 0
    let types = [Checkerboard, ColumnInterleaved, RowInterleaved, SideBySide, TopBottom, FrameSequential, Mono, TileFormat];
    for (t, packing) in types.into_iter().enumerate() {
        for ext in ["mp4", "mkv", "ts"] {
            // the tile format needs a frame size divisible by 3
            let size = if packing == TileFormat { "testsrc2=size=96x48:rate=25" } else { "testsrc2=size=64x32:rate=25" };
            let params = format!("frame-packing={t}");
            let mut args = vec!["-f", "lavfi", "-i", size, "-frames:v", "4", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-x264-params", &params];
            if ext == "ts" {
                args.extend(["-f", "mpegts"]);
            }
            let Some(p) = fixture(&ff, &format!("stereo_x264_fpa{t}.{ext}"), &args) else { continue };
            let l = stereo(&p).unwrap_or_else(|| panic!("{}: no stereo layout", p.display()));
            assert_eq!((l.packing, l.mark), (packing, StereoMark::FramePackingSei), "{}", p.display());
            if packing != Mono {
                assert_eq!(l.first, Eye::Left, "{}", p.display());
            }
            // 64×32: a side-by-side view would be 32×32 at full resolution (narrower than 5:4),
            // a top-bottom one 64×16 (wider than 5:2), so both read as half resolution
            assert_eq!(l.half, matches!(packing, SideBySide | TopBottom), "{}", p.display());
            assert!(!l.quincunx);
        }
    }
}

/// H.265 prefix SEI NAL unit with a frame packing arrangement message (D.2.16): grid positions
/// (0, 0) for types other than 5, persistence 1, upsampled_aspect_ratio_flag 0.
fn hevc_fpa(arrangement_type: u8, content: u8) -> Vec<u8> {
    let mut p = filmcraft_bitstream::BitWriter::new();
    p.write_ue(0);
    p.write_bit(false);
    p.write_bits(arrangement_type as u32, 7);
    p.write_bit(false);
    p.write_bits(content as u32, 6);
    p.write_bits(0, 3);
    p.write_bit(arrangement_type == 5);
    p.write_bits(0, 2);
    if arrangement_type != 5 {
        p.write_bits(0, 16);
    }
    p.write_bits(0, 8);
    p.write_bit(true);
    p.write_bit(false);
    if !p.is_byte_aligned() {
        p.write_bit(true);
    }
    sei_nal(45, p.finish())
}

/// H.265 prefix SEI NAL unit with a segmented rectangular frame packing message (D.2.29).
fn hevc_srfpa(content: u8) -> Vec<u8> {
    // cancel 0, content (2 bits), persistence 1, then the alignment bits 1 000
    sei_nal(138, vec![(content << 5) | 0x18])
}

fn sei_nal(payload_type: u8, payload: Vec<u8>) -> Vec<u8> {
    let mut rbsp = vec![payload_type, payload.len() as u8];
    rbsp.extend(payload);
    rbsp.push(0x80);
    let mut nal = vec![0x4E, 0x01];
    nal.extend(filmcraft_bitstream::escape_rbsp(&rbsp));
    nal
}

/// `stream` (Annex B H.265) with `sei` before the first slice segment of every picture.
fn with_sei(stream: &[u8], sei: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in filmcraft_bitstream::annexb_nals(stream) {
        let kind = (nal[0] >> 1) & 0x3f;
        // a VCL NAL unit with first_slice_segment_in_pic_flag set starts a picture
        if kind < 32 && nal.get(2).is_some_and(|b| b & 0x80 != 0) {
            out.extend([0, 0, 0, 1]);
            out.extend(sei);
        }
        out.extend([0, 0, 0, 1]);
        out.extend(nal);
    }
    out
}

/// FFmpeg's Stereo 3D side data of each decoded frame of `p` (its showinfo filter's lines).
fn ffmpeg_stereo(ff: &Path, p: &Path) -> Vec<String> {
    let out = Command::new(ff).args(["-hide_banner", "-nostdin", "-i"]).arg(p).args(["-vf", "showinfo", "-f", "null", "-"]).output().expect("ffmpeg");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stderr).lines().filter(|l| l.contains("Parsed_showinfo") && l.contains("Stereo 3D:")).map(str::to_string).collect()
}

#[test]
fn hevc_frame_packing_sei() {
    let ff = filmcraft_testkit::require_ffmpeg!();
    let mut args = V64X32.to_vec();
    // no B-frames: a raw stream's timestamps are then those of its decoding order, which every
    // muxer accepts
    args.extend(["-c:v", "libx265", "-x265-params", "bframes=0:log-level=error", "-f", "hevc"]);
    let Some(base) = fixture(&ff, "stereo_x265_base_ipp.hevc", &args) else { return };
    let base = std::fs::read(base).unwrap();
    // (name, SEI, our layout, what FFmpeg's decoder reports for each frame)
    let cases: Vec<(&str, Vec<u8>, StereoPacking, Eye, Option<&str>)> = vec![
        ("sbs_left", hevc_fpa(3, 1), SideBySide, Eye::Left, Some("type - side by side,")),
        ("sbs_right", hevc_fpa(3, 2), SideBySide, Eye::Right, Some("type - side by side (inverted),")),
        ("tb_left", hevc_fpa(4, 1), TopBottom, Eye::Left, Some("type - top and bottom,")),
        ("tb_right", hevc_fpa(4, 2), TopBottom, Eye::Right, Some("type - top and bottom (inverted),")),
        ("frames", hevc_fpa(5, 1), FrameSequential, Eye::Left, Some("type - frame alternate,")),
        ("tiles", hevc_srfpa(1), TileFormat, Eye::Left, None),
        ("tiles_right", hevc_srfpa(2), TileFormat, Eye::Right, None),
    ];
    for (name, sei, packing, first, ffmpeg_reads) in cases {
        let es = fixture_dir_file(&format!("stereo_x265_{name}.hevc"));
        std::fs::write(&es, with_sei(&base, &sei)).unwrap();
        // FFmpeg's HEVC decoder reads the frame packing messages it knows (not the segmented one)
        if let Some(want) = ffmpeg_reads {
            let lines = ffmpeg_stereo(&ff, &es);
            assert_eq!(lines.iter().filter(|l| l.contains(want)).count(), 4, "{name}: FFmpeg read {lines:#?}");
        }
        for ext in ["mp4", "mkv", "ts"] {
            // a raw stream has no timestamps: FFmpeg makes them at 25 fps
            let mut args = vec!["-fflags", "+genpts", "-f", "hevc", "-r", "25", "-i", es.to_str().unwrap(), "-c", "copy"];
            if ext == "ts" {
                args.extend(["-f", "mpegts"]);
            }
            let p = fixture(&ff, &format!("stereo_x265_{name}.{ext}"), &args).expect("remux with ffmpeg");
            let l = stereo(&p).unwrap_or_else(|| panic!("{}: no stereo layout", p.display()));
            assert_eq!((l.packing, l.first, l.mark), (packing, first, StereoMark::FramePackingSei), "{}", p.display());
        }
    }
}

fn fixture_dir_file(name: &str) -> PathBuf {
    let d = dir();
    std::fs::create_dir_all(&d).unwrap();
    d.join(name)
}

#[test]
fn matroska_stereo_modes_written_by_ffmpeg() {
    let ff = filmcraft_testkit::require_ffmpeg!();
    // FFmpeg's names for RFC 9559 Table 5, in value order
    let names = [
        "mono",
        "left_right",
        "bottom_top",
        "top_bottom",
        "checkerboard_rl",
        "checkerboard_lr",
        "row_interleaved_rl",
        "row_interleaved_lr",
        "col_interleaved_rl",
        "col_interleaved_lr",
        "anaglyph_cyan_red",
        "right_left",
        "anaglyph_green_magenta",
        "block_lr",
        "block_rl",
    ];
    for (mode, name) in names.iter().enumerate() {
        let meta = format!("stereo_mode={name}");
        let mut args = V64X32.to_vec();
        args.extend(["-c:v", "mjpeg", "-metadata:s:v", &meta]);
        let Some(p) = fixture(&ff, &format!("stereo_mode_{mode}.mkv"), &args) else { continue };
        let l = stereo(&p).unwrap_or_else(|| panic!("{name}: no stereo layout"));
        assert_eq!(Some(l), StereoLayout::from_matroska(mode as u64).map(|m| StereoLayout { half: l.half, ..m }), "{name}");
        // FFmpeg writes a plane's stored size as its display size, so (RFC 9559 §18.10) its
        // side-by-side and top-bottom views are full resolution as stored
        assert!(!l.half, "{name}");
    }
}

#[test]
fn st3d_written_by_ffmpeg() {
    let ff = filmcraft_testkit::require_ffmpeg!();
    for (name, mode, packing, first) in [("left_right", 2, SideBySide, Eye::Left), ("top_bottom", 1, TopBottom, Eye::Left)] {
        let meta = format!("stereo_mode={name}");
        let mut args = V64X32.to_vec();
        args.extend(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-metadata:s:v", &meta]);
        let Some(mkv) = fixture(&ff, &format!("stereo_st3d_src_{name}.mkv"), &args) else { continue };
        // FFmpeg's MP4 muxer writes st3d from the Matroska stereo mode (an "unofficial" box to it)
        let Some(mp4) = fixture(&ff, &format!("stereo_st3d_{name}.mp4"), &["-i", mkv.to_str().unwrap(), "-c", "copy", "-strict", "unofficial"]) else {
            continue;
        };
        let b = std::fs::read(&mp4).unwrap();
        let at = b.windows(4).position(|w| w == b"st3d").expect("ffmpeg wrote an st3d box");
        assert_eq!(b[at + 8], mode);
        let l = stereo(&mp4).unwrap();
        assert_eq!((l.packing, l.first, l.half, l.mark), (packing, first, false, StereoMark::St3d), "{name}");
    }
}

#[test]
fn no_mark_no_layout() {
    let ff = filmcraft_testkit::require_ffmpeg!();
    let mut args = V64X32.to_vec();
    args.extend(["-c:v", "libx264", "-pix_fmt", "yuv420p"]);
    let Some(p) = fixture(&ff, "stereo_none.mp4", &args) else { return };
    assert_eq!(stereo(&p), None);
}
