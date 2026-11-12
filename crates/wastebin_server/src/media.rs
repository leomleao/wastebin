//! Helpers for recognizing and formatting uploaded media (images and PDFs).

pub fn is_image_ext(ext: Option<&str>) -> bool {
    matches!(
        ext.map(|s| s.to_ascii_lowercase()).as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "bmp" | "ico" | "tiff" | "tif")
    )
}

pub fn is_pdf_ext(ext: Option<&str>) -> bool {
    matches!(ext.map(|s| s.to_ascii_lowercase()).as_deref(), Some("pdf"))
}

/// MIME type of an asset uploaded from the pad editor, from its extension or, lacking one, from
/// the magic bytes of the binary formats the editor accepts. Must only be used for entries
/// stored as assets: text pastes are always served as plain text.
pub fn asset_mime(bytes: &[u8], ext: Option<&str>) -> &'static str {
    if let Some(ext) = ext {
        return match ext.to_ascii_lowercase().as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "svg" => "image/svg+xml",
            "avif" => "image/avif",
            "bmp" => "image/bmp",
            "ico" => "image/x-icon",
            "tiff" | "tif" => "image/tiff",
            "pdf" => "application/pdf",
            _ => "application/octet-stream",
        };
    }

    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else if bytes.starts_with(b"%PDF-") {
        "application/pdf"
    } else {
        "application/octet-stream"
    }
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
    fn test_is_image_ext() {
        assert!(is_image_ext(Some("png")));
        assert!(is_image_ext(Some("jpg")));
        assert!(is_image_ext(Some("jpeg")));
        assert!(is_image_ext(Some("gif")));
        assert!(is_image_ext(Some("webp")));
        assert!(is_image_ext(Some("avif")));
        assert!(is_image_ext(Some("bmp")));
        assert!(is_image_ext(Some("ico")));
        assert!(is_image_ext(Some("tiff")));
        assert!(is_image_ext(Some("tif")));

        // svg is not considered an image extension (it's text/code)
        assert!(!is_image_ext(Some("svg")));
        assert!(!is_image_ext(Some("html")));
        assert!(!is_image_ext(Some("js")));
        assert!(!is_image_ext(Some("rs")));
        assert!(!is_image_ext(None));
    }

    #[test]
    fn asset_mime_from_extension() {
        assert_eq!(asset_mime(b"", Some("png")), "image/png");
        assert_eq!(asset_mime(b"", Some("JPG")), "image/jpeg");
        assert_eq!(asset_mime(b"", Some("svg")), "image/svg+xml");
        assert_eq!(asset_mime(b"", Some("tif")), "image/tiff");
        assert_eq!(asset_mime(b"", Some("pdf")), "application/pdf");
        // Executable or rich text formats never get an executable MIME type.
        assert_eq!(
            asset_mime(b"<html>", Some("html")),
            "application/octet-stream"
        );
        assert_eq!(
            asset_mime(b"alert(1)", Some("js")),
            "application/octet-stream"
        );
    }

    #[test]
    fn asset_mime_from_magic_bytes() {
        assert_eq!(asset_mime(b"\x89PNG\r\n\x1a\nrest", None), "image/png");
        assert_eq!(asset_mime(b"%PDF-1.4", None), "application/pdf");
        // Text that merely starts like a bitmap or an SVG is not trusted.
        assert_eq!(
            asset_mime(b"BMW is a car", None),
            "application/octet-stream"
        );
        assert_eq!(
            asset_mime(b"<svg onload=alert(1)>", None),
            "application/octet-stream"
        );
    }
}
