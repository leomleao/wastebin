//! Media types of the images and PDFs uploaded from the pad editor.

/// Extensions and MIME types of the files the pad editor accepts.
const TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("svg", "image/svg+xml"),
    ("avif", "image/avif"),
    ("bmp", "image/bmp"),
    ("ico", "image/x-icon"),
    ("tiff", "image/tiff"),
    ("pdf", "application/pdf"),
];

/// Extension to store an upload of MIME type `mime` under, `None` if it is not accepted.
pub fn ext_for_mime(mime: &str) -> Option<&'static str> {
    let mime = mime.split(';').next().unwrap_or_default().trim();
    match mime.to_ascii_lowercase().as_str() {
        "image/jpg" => Some("jpg"),
        "image/vnd.microsoft.icon" => Some("ico"),
        mime => TYPES.iter().find(|(_, m)| *m == mime).map(|(ext, _)| *ext),
    }
}

/// MIME type of an asset with extension `ext`. Must only be used for entries stored as assets:
/// text pastes are always served as plain text.
pub fn asset_mime(ext: Option<&str>) -> &'static str {
    let ext = ext.unwrap_or_default().to_ascii_lowercase();
    let ext = match ext.as_str() {
        "jpeg" => "jpg",
        "tif" => "tiff",
        ext => ext,
    };
    TYPES
        .iter()
        .find(|(e, _)| *e == ext)
        .map_or("application/octet-stream", |(_, mime)| *mime)
}

pub fn format_size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_and_extension_round_trip() {
        for (ext, mime) in TYPES {
            assert_eq!(ext_for_mime(mime), Some(*ext));
            assert_eq!(asset_mime(Some(ext)), *mime);
        }
        assert_eq!(ext_for_mime("IMAGE/JPG"), Some("jpg"));
        assert_eq!(asset_mime(Some("JPEG")), "image/jpeg");
        assert_eq!(asset_mime(Some("tif")), "image/tiff");
    }

    #[test]
    fn rejects_other_types() {
        assert_eq!(ext_for_mime("text/html"), None);
        assert_eq!(ext_for_mime("application/javascript"), None);
        // Executable or rich text formats never get an executable MIME type.
        assert_eq!(asset_mime(Some("html")), "application/octet-stream");
        assert_eq!(asset_mime(Some("js")), "application/octet-stream");
        assert_eq!(asset_mime(None), "application/octet-stream");
    }
}
