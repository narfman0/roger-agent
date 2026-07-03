//! Attachment handling: turn a non-text Matrix message (image / file / video)
//! into structured metadata, decide eager-vs-lazy download, and (for eager)
//! persist the bytes to a per-room directory so the coding agent can reach it.
//!
//! Audio is intentionally *not* handled here — it keeps its existing
//! transcription path in `handler.rs`.
//!
//! The metadata extraction (`attachment_meta`) is pure and matchable so it can be
//! unit-tested without a live client. Downloading (`download_to`) reuses the same
//! `media().get_media_content(...)` primitive the audio path uses, which
//! transparently decrypts E2EE attachments when the client holds the room keys.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use matrix_sdk::{
    media::{MediaFormat, MediaRequestParameters},
    ruma::events::room::{message::MessageType, MediaSource},
    Client,
};

/// Which kind of attachment this is. Audio is handled separately (transcription).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    Image,
    File,
    Video,
}

impl AttachmentKind {
    fn label(self) -> &'static str {
        match self {
            AttachmentKind::Image => "image",
            AttachmentKind::File => "file",
            AttachmentKind::Video => "video",
        }
    }
}

/// Structured metadata extracted from a file-bearing message, plus the download
/// source. Everything but `source` is derivable without touching the network.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub kind: AttachmentKind,
    pub filename: String,
    pub mimetype: Option<String>,
    pub size_bytes: Option<u64>,
    /// Image/video pixel dimensions, if known.
    pub width: Option<u64>,
    pub height: Option<u64>,
    /// Audio/video duration, if known.
    pub duration: Option<Duration>,
    /// Optional user-written caption (distinct from the filename).
    pub caption: Option<String>,
    /// The mxc/encrypted source used to fetch (and decrypt) the bytes.
    pub source: MediaSource,
}

/// Extract attachment metadata from a message type. Returns `None` for text and
/// audio (audio has its own transcription path) and any non-file message.
pub fn attachment_meta(msgtype: &MessageType) -> Option<Attachment> {
    match msgtype {
        MessageType::Image(c) => {
            let info = c.info.as_deref();
            Some(Attachment {
                kind: AttachmentKind::Image,
                filename: c.filename().to_string(),
                mimetype: info.and_then(|i| i.mimetype.clone()),
                size_bytes: info.and_then(|i| i.size).map(u64::from),
                width: info.and_then(|i| i.width).map(u64::from),
                height: info.and_then(|i| i.height).map(u64::from),
                duration: None,
                caption: c.caption().map(str::to_string),
                source: c.source.clone(),
            })
        }
        MessageType::Video(c) => {
            let info = c.info.as_deref();
            Some(Attachment {
                kind: AttachmentKind::Video,
                filename: c.filename().to_string(),
                mimetype: info.and_then(|i| i.mimetype.clone()),
                size_bytes: info.and_then(|i| i.size).map(u64::from),
                width: info.and_then(|i| i.width).map(u64::from),
                height: info.and_then(|i| i.height).map(u64::from),
                duration: info.and_then(|i| i.duration),
                caption: c.caption().map(str::to_string),
                source: c.source.clone(),
            })
        }
        MessageType::File(c) => {
            let info = c.info.as_deref();
            Some(Attachment {
                kind: AttachmentKind::File,
                filename: c.filename().to_string(),
                mimetype: info.and_then(|i| i.mimetype.clone()),
                size_bytes: info.and_then(|i| i.size).map(u64::from),
                width: None,
                height: None,
                duration: None,
                caption: c.caption().map(str::to_string),
                source: c.source.clone(),
            })
        }
        _ => None,
    }
}

/// Whether a File attachment's mime type is a known-safe, text-ish payload worth
/// pulling eagerly. Arbitrary/opaque types are left for lazy fetch.
fn is_safe_file_mime(mime: &str) -> bool {
    let m = mime.split(';').next().unwrap_or(mime).trim();
    m.starts_with("text/")
        || matches!(
            m,
            "application/pdf"
                | "application/json"
                | "application/xml"
                | "application/toml"
                | "application/x-toml"
                | "application/yaml"
                | "application/x-yaml"
                | "application/csv"
                | "application/x-sh"
                | "application/javascript"
        )
}

impl Attachment {
    /// Eager-small / lazy-large policy:
    /// - images: eager when within the size cap;
    /// - files: eager when within the cap *and* a known-safe text-ish mime;
    /// - videos: always lazy (media, typically large).
    ///
    /// Unknown size ⇒ lazy (we can't bound the download).
    pub fn should_download_eager(&self, max_eager_bytes: u64) -> bool {
        let within_cap = self.size_bytes.map(|s| s <= max_eager_bytes).unwrap_or(false);
        match self.kind {
            AttachmentKind::Image => within_cap,
            AttachmentKind::Video => false,
            AttachmentKind::File => {
                within_cap
                    && self
                        .mimetype
                        .as_deref()
                        .map(is_safe_file_mime)
                        .unwrap_or(false)
            }
        }
    }

    /// Download (and decrypt, if E2EE) the bytes into `dir`, returning the saved
    /// path. The filename is sanitized to a basename and prefixed with `unique`
    /// to avoid collisions between messages that share a filename.
    pub async fn download_to(&self, client: &Client, dir: &Path, unique: &str) -> Result<PathBuf> {
        let bytes = client
            .media()
            .get_media_content(
                &MediaRequestParameters {
                    source: self.source.clone(),
                    format: MediaFormat::File,
                },
                true,
            )
            .await?;

        std::fs::create_dir_all(dir)?;
        let safe = sanitize_filename(&self.filename);
        let name = format!("{}-{}", unique, safe);
        let path = dir.join(name);
        std::fs::write(&path, bytes)?;
        Ok(path)
    }

    /// A one-line, machine-readable + human-readable description used both as the
    /// persisted history line and the basis for the ack. `saved` is the on-disk
    /// path when eagerly downloaded, else `None` (lazy).
    pub fn summary_line(&self, saved: Option<&Path>) -> String {
        let mut meta = Vec::new();
        if let Some(m) = &self.mimetype {
            meta.push(m.clone());
        }
        if let Some(s) = self.size_bytes {
            meta.push(human_size(s));
        }
        if let (Some(w), Some(h)) = (self.width, self.height) {
            meta.push(format!("{}x{}", w, h));
        }
        if let Some(d) = self.duration {
            meta.push(format!("{}s", d.as_secs()));
        }
        let meta = if meta.is_empty() {
            String::new()
        } else {
            format!(" ({})", meta.join(", "))
        };
        let location = match saved {
            Some(p) => format!(" saved to {}", p.display()),
            None => " (not downloaded yet; fetch on request)".to_string(),
        };
        format!(
            "[attachment] {} \"{}\"{}{}",
            self.kind.label(),
            self.filename,
            meta,
            location
        )
    }

    /// Short, friendly acknowledgement for a bare upload (no caption, idle room).
    pub fn ack_line(&self, saved: Option<&Path>) -> String {
        let mut meta = Vec::new();
        if let Some(m) = &self.mimetype {
            meta.push(m.clone());
        }
        if let Some(s) = self.size_bytes {
            meta.push(human_size(s));
        }
        let meta = if meta.is_empty() {
            String::new()
        } else {
            format!(" ({})", meta.join(", "))
        };
        match saved {
            Some(p) => format!(
                "I see your {} `{}`{} at `{}`. What would you like me to do with it?",
                self.kind.label(),
                self.filename,
                meta,
                p.display()
            ),
            None => format!(
                "I see your {} `{}`{}. It's large/opaque so I haven't pulled it down yet — tell me what to do with it and I'll fetch it.",
                self.kind.label(),
                self.filename,
                meta
            ),
        }
    }
}

/// Reduce a user-supplied filename to a safe basename (no path separators, no
/// parent refs). Falls back to `attachment` if nothing usable remains.
fn sanitize_filename(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim()
        .trim_matches('.');
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect();
    if cleaned.is_empty() {
        "attachment".to_string()
    } else {
        cleaned
    }
}

/// Human-friendly byte size (B / KB / MB / GB), matching common client display.
fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::events::room::{
        message::{
            FileInfo, FileMessageEventContent, ImageMessageEventContent, VideoInfo,
            VideoMessageEventContent,
        },
        ImageInfo,
    };
    use matrix_sdk::ruma::{mxc_uri, UInt};

    fn image_msg(filename: &str, caption: Option<&str>, size: Option<u64>, dims: Option<(u64, u64)>) -> MessageType {
        let mut c = ImageMessageEventContent::plain(
            caption.unwrap_or(filename).to_string(),
            mxc_uri!("mxc://example.org/abc123").to_owned(),
        );
        if caption.is_some() {
            c.filename = Some(filename.to_string());
        }
        let mut info = ImageInfo::new();
        info.mimetype = Some("image/png".to_string());
        info.size = size.and_then(|s| UInt::try_from(s).ok());
        if let Some((w, h)) = dims {
            info.width = UInt::try_from(w).ok();
            info.height = UInt::try_from(h).ok();
        }
        c.info = Some(Box::new(info));
        MessageType::Image(c)
    }

    fn file_msg(filename: &str, mime: &str, size: u64) -> MessageType {
        let mut c = FileMessageEventContent::plain(
            filename.to_string(),
            mxc_uri!("mxc://example.org/file1").to_owned(),
        );
        let mut info = FileInfo::new();
        info.mimetype = Some(mime.to_string());
        info.size = UInt::try_from(size).ok();
        c.info = Some(Box::new(info));
        MessageType::File(c)
    }

    fn video_msg(filename: &str, size: u64) -> MessageType {
        let mut c = VideoMessageEventContent::plain(
            filename.to_string(),
            mxc_uri!("mxc://example.org/vid1").to_owned(),
        );
        let mut info = VideoInfo::new();
        info.mimetype = Some("video/mp4".to_string());
        info.size = UInt::try_from(size).ok();
        c.info = Some(Box::new(info));
        MessageType::Video(c)
    }

    #[test]
    fn extracts_image_metadata_and_caption() {
        let att = attachment_meta(&image_msg("seton.png", Some("look at this"), Some(1_200_000), Some((1920, 1080)))).unwrap();
        assert_eq!(att.kind, AttachmentKind::Image);
        assert_eq!(att.filename, "seton.png");
        assert_eq!(att.mimetype.as_deref(), Some("image/png"));
        assert_eq!(att.size_bytes, Some(1_200_000));
        assert_eq!((att.width, att.height), (Some(1920), Some(1080)));
        assert_eq!(att.caption.as_deref(), Some("look at this"));
    }

    #[test]
    fn bare_image_has_no_caption() {
        let att = attachment_meta(&image_msg("seton.png", None, Some(1000), None)).unwrap();
        assert_eq!(att.filename, "seton.png");
        assert!(att.caption.is_none());
    }

    #[test]
    fn text_and_unknown_yield_none() {
        use matrix_sdk::ruma::events::room::message::TextMessageEventContent;
        assert!(attachment_meta(&MessageType::Text(TextMessageEventContent::plain("hi"))).is_none());
    }

    #[test]
    fn eager_policy_small_image_yes_video_no() {
        let cap = 10 * 1024 * 1024;
        assert!(attachment_meta(&image_msg("a.png", None, Some(2_000_000), None)).unwrap().should_download_eager(cap));
        // Oversized image → lazy.
        assert!(!attachment_meta(&image_msg("big.png", None, Some(50_000_000), None)).unwrap().should_download_eager(cap));
        // Unknown size → lazy.
        assert!(!attachment_meta(&image_msg("u.png", None, None, None)).unwrap().should_download_eager(cap));
        // Video always lazy.
        assert!(!attachment_meta(&video_msg("clip.mp4", 1000)).unwrap().should_download_eager(cap));
    }

    #[test]
    fn eager_policy_file_depends_on_mime() {
        let cap = 10 * 1024 * 1024;
        assert!(attachment_meta(&file_msg("notes.txt", "text/plain", 1000)).unwrap().should_download_eager(cap));
        assert!(attachment_meta(&file_msg("doc.pdf", "application/pdf", 1000)).unwrap().should_download_eager(cap));
        // Arbitrary/opaque binary → lazy even when small.
        assert!(!attachment_meta(&file_msg("blob.bin", "application/octet-stream", 1000)).unwrap().should_download_eager(cap));
    }

    #[test]
    fn summary_line_shapes() {
        let att = attachment_meta(&image_msg("seton.png", None, Some(1_200_000), Some((1920, 1080)))).unwrap();
        let saved = PathBuf::from("/data/att/seton.png");
        let line = att.summary_line(Some(&saved));
        assert!(line.contains("[attachment] image \"seton.png\""));
        assert!(line.contains("image/png"));
        assert!(line.contains("1920x1080"));
        assert!(line.contains("saved to /data/att/seton.png"));
        // Lazy variant notes it isn't downloaded.
        assert!(att.summary_line(None).contains("not downloaded"));
    }

    #[test]
    fn sanitize_strips_paths() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("a b.png"), "a_b.png");
        assert_eq!(sanitize_filename("/"), "attachment");
    }

    #[test]
    fn human_size_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(1_572_864), "1.5 MB");
    }
}
