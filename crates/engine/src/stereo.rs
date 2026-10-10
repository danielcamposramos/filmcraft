//! Stereo 3D media in a session.
//!
//! A media item marked stereo (frame packing SEI, Matroska `StereoMode`, `st3d`; see
//! `filmcraft_codecs::stereo`), or interpreted as stereo (Interpret Footage ▸ Stereoscopic 3D), is
//! read through [`filmcraft_media::stereo::SideBySideSource`]: its frames are full side by side
//! with the left eye first and its `info` describes them. A sequence made from it is a stereo
//! sequence ([`filmcraft_project::SequenceSettings::stereo`]), whose exports carry the stereo mark;
//! VR effects applied to it start with the side-by-side frame layout.
//!
//! - [`delivered`]: a file's source read with Interpret Footage's layout, else the file's mark.
//! - `read` / `apply`: the stereo part of `clip.interpretFootage`.
//! - `media.stereoInfo`: the file's mark, the override and the layouts Interpret Footage offers.

use filmcraft_media::SharedSource;
use filmcraft_media::stereo::StereoLayout;
use filmcraft_project::{ItemId, ItemKind, MediaRef, Project};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, always, bad, item_p};
use crate::{Result, Session};

/// `src`, as opened from its file, read with Interpret Footage's layout `interpret` (None: the
/// layout the file is marked with).
pub fn delivered(src: SharedSource, interpret: Option<StereoLayout>) -> SharedSource {
    let own = src.info().video.as_ref().and_then(|v| v.stereo);
    filmcraft_media::stereo::wrap(src, interpret.or(own))
}

/// Whether an item's picture is a stereo pair, full side by side: stereo media (or a subclip of
/// it) or a stereo sequence.
pub fn item_is_stereo(p: &Project, item: ItemId) -> bool {
    match p.item(item).map(|i| &i.kind) {
        Some(ItemKind::Sequence(q)) => q.settings.stereo,
        _ => p.resolve_media(item).is_some_and(|(_, m, _)| m.stereo().is_some()),
    }
}

/// The video stream info of media `items` read with `layout` (None: as their files are marked),
/// for [`apply`]: each file is opened to learn its stored size and rate.
pub(crate) fn read(s: &Session, items: &[ItemId], layout: Option<StereoLayout>) -> Result<Vec<(ItemId, filmcraft_media::VideoStreamInfo)>> {
    let mut out = Vec::new();
    for &i in items {
        let Some(ItemKind::Media(m)) = s.project.item(i).map(|it| &it.kind) else { continue };
        let MediaRef::File { path } = &m.media else {
            return Err(bad("clip.interpretFootage", "only media files have a stereo layout"));
        };
        let raw = s.media.probe_file(path, &*s.services).map_err(|e| bad("clip.interpretFootage", format!("{path}: {e}")))?;
        if let Some(v) = delivered(raw, layout).info().video.clone() {
            out.push((i, v));
        }
    }
    Ok(out)
}

/// Interpret Footage ▸ Stereoscopic 3D: the items [`read`] found are read with `layout` from now on.
/// Their video info describes the frames read that way, so their clips, thumbnails and sequences
/// follow (the media pool's cache key changes with it).
pub(crate) fn apply(pr: &mut Project, reads: &[(ItemId, filmcraft_media::VideoStreamInfo)], layout: Option<StereoLayout>) {
    for (i, v) in reads {
        if let Some(ItemKind::Media(m)) = pr.item_mut(*i).map(|it| &mut it.kind) {
            m.interpret.stereo = layout;
            let still = m.info.kind == filmcraft_media::MediaKind::Still;
            if let Some(mv) = m.info.video.as_mut() {
                (mv.width, mv.height, mv.stereo) = (v.width, v.height, v.stereo);
                // a still's rate is the indeterminate media timebase, not the file's
                if !still {
                    mv.frame_rate = v.frame_rate;
                }
            }
        }
    }
}

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "media.stereoInfo",
        label: "Media Stereo 3D Info",
        menu: &[],
        shortcut: None,
        params: r#"{"item":id}"#,
        enabled: always,
        run: stereo_info,
        journal: false,
    }]
}

fn layout_json(l: Option<StereoLayout>) -> Value {
    l.map_or(Value::Null, |l| json!({"id": l.id(), "label": l.label(), "stereo": l.is_stereo()}))
}

/// `media.stereoInfo`: the layout the file is marked with (`file`; the file is opened when an
/// override hides it), Interpret Footage's override, the layout in effect, and the layouts
/// Interpret Footage offers.
fn stereo_info(s: &mut Session, p: &Value) -> Result<Value> {
    let id = item_p(p, "item").ok_or_else(|| bad("media.stereoInfo", "need `item`"))?;
    let Some(ItemKind::Media(m)) = s.project.item(id).map(|i| &i.kind) else { return Err(bad("media.stereoInfo", "not a media item")) };
    let effective = m.info.video.as_ref().and_then(|v| v.stereo);
    let file = match &m.media {
        MediaRef::File { path } => match s.media.probe_file(path, &*s.services) {
            Ok(src) => src.info().video.as_ref().and_then(|v| v.stereo),
            Err(_) if m.interpret.stereo.is_none() => effective,
            Err(_) => None,
        },
        MediaRef::Generator(_) => None,
    };
    let choices: Vec<Value> = StereoLayout::choices().into_iter().map(|l| json!({"id": l.id(), "label": l.describe()})).collect();
    Ok(json!({
        "file": layout_json(file),
        "override": m.interpret.stereo.map(|l| l.id()),
        "effective": layout_json(effective),
        "choices": choices,
    }))
}
