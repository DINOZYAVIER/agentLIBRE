//! What a surface does with the media of a message (AGL-014, D18, D21;
//! AGL-127, D2, D9): the files the human gives are read at once and get
//! their placeholder, numbered after the media the session holds and those
//! waiting; the daemon stores them when the message that names them is sent.

use agentlibre_common::media::{Kind, Pending, Source};
use agentlibre_proto as proto;

/// Adds `source` to `pending` after `counts` media of each kind the session
/// holds, in the order of [`Kind::ALL`] (which is the order of
/// `SessionView.media`), and returns its placeholder.
pub fn add_counted(counts: &[u32; 3], pending: &mut Vec<Pending>, source: Source) -> String {
    let index = Kind::ALL.iter().position(|kind| *kind == source.kind).unwrap_or(0);
    let number = counts[index]
        + pending.iter().filter(|p| p.source.kind == source.kind).count() as u32
        + 1;
    let item = Pending { number, source };
    let placeholder = item.placeholder();
    pending.push(item);
    placeholder
}

/// The placeholders and the message: `[Image#1] [Audio#1] message` (D18).
pub fn with_placeholders(placeholders: &[String], message: &str) -> String {
    if placeholders.is_empty() {
        message.to_owned()
    } else {
        format!("{} {message}", placeholders.join(" "))
    }
}

/// A medium waiting to be sent, its bytes in base64 (D14). `Say` carries
/// every pending medium: a placeholder the human deleted sends nothing
/// because the daemon's `store` drops it (AGL-014, D21).
pub fn attached(pending: &Pending) -> proto::Attached {
    proto::Attached {
        number: pending.number,
        kind: kind(pending.source.kind),
        name: pending.source.name.clone(),
        ext: pending.source.ext.clone(),
        base64: proto::base64::encode(&pending.source.bytes),
    }
}

/// The protocol's kind of a medium (AGL-127, D9).
pub fn kind(kind: Kind) -> proto::MediaKind {
    match kind {
        Kind::Image => proto::MediaKind::Image,
        Kind::Video => proto::MediaKind::Video,
        Kind::Audio => proto::MediaKind::Audio,
    }
}

/// The kind of a medium of the protocol's (AGL-127, D9).
pub fn kind_of(kind: proto::MediaKind) -> Kind {
    match kind {
        proto::MediaKind::Image => Kind::Image,
        proto::MediaKind::Video => Kind::Video,
        proto::MediaKind::Audio => Kind::Audio,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(kind: Kind, name: &str) -> Source {
        Source {
            kind,
            bytes: name.as_bytes().to_vec(),
            ext: match kind {
                Kind::Image => "png",
                Kind::Video => "mp4",
                Kind::Audio => "wav",
            }
            .into(),
            name: name.into(),
        }
    }

    #[test]
    fn the_numbers_follow_the_session_and_the_pending_media_by_kind() {
        let mut pending = Vec::new();
        let none = [0, 0, 0];
        assert_eq!(add_counted(&none, &mut pending, source(Kind::Image, "a.png")), "[Image#1]");
        assert_eq!(add_counted(&none, &mut pending, source(Kind::Image, "b.png")), "[Image#2]");
        assert_eq!(add_counted(&none, &mut pending, source(Kind::Audio, "v.wav")), "[Audio#1]");
        // The session already holds one image: the next is the second.
        let mut more = Vec::new();
        assert_eq!(add_counted(&[1, 0, 0], &mut more, source(Kind::Image, "c.png")), "[Image#2]");
        // By kind, in the order of `Kind::ALL`: image, video, audio.
        assert_eq!(add_counted(&[0, 4, 7], &mut more, source(Kind::Video, "d.mp4")), "[Video#5]");
        assert_eq!(add_counted(&[0, 4, 7], &mut more, source(Kind::Audio, "e.wav")), "[Audio#8]");
        assert_eq!(
            with_placeholders(&["[Image#1]".into(), "[Audio#1]".into()], "hi"),
            "[Image#1] [Audio#1] hi"
        );
        assert_eq!(with_placeholders(&[], "hi"), "hi");
    }

    #[test]
    fn a_medium_goes_as_base64() {
        let pending = Pending {
            number: 2,
            source: Source {
                kind: Kind::Audio,
                bytes: vec![0, 1, 2, 255],
                ext: "wav".into(),
                name: "said.wav".into(),
            },
        };
        assert_eq!(
            attached(&pending),
            proto::Attached {
                number: 2,
                kind: proto::MediaKind::Audio,
                name: "said.wav".into(),
                ext: "wav".into(),
                base64: "AAEC/w==".into(),
            }
        );
    }

    #[test]
    fn a_kind_goes_to_the_protocol_and_back() {
        for k in Kind::ALL {
            assert_eq!(kind_of(kind(k)), k);
        }
        assert_eq!(kind(Kind::Video), proto::MediaKind::Video);
    }
}
