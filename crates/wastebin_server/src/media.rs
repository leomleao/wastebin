/// Helper functions for recognizing and formatting media (images, PDFs, text).

pub fn is_image_ext(ext: Option<&str>) -> bool {
    matches!(
        ext.map(|s| s.to_ascii_lowercase()).as_deref(),
        Some(
            "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "webp"
                | "avif"
                | "bmp"
                | "ico"
                | "tiff"
                | "tif"
        )
    )
}

pub fn is_pdf_ext(ext: Option<&str>) -> bool {
    matches!(
        ext.map(|s| s.to_ascii_lowercase()).as_deref(),
        Some("pdf")
    )
}

pub fn detect_mime(bytes: &[u8], ext: Option<&str>) -> &'static str {
    if let Some(ext) = ext {
        match ext.to_ascii_lowercase().as_str() {
            "png" => return "image/png",
            "jpg" | "jpeg" => return "image/jpeg",
            "gif" => return "image/gif",
            "webp" => return "image/webp",
            "svg" => return "image/svg+xml",
            "avif" => return "image/avif",
            "bmp" => return "image/bmp",
            "ico" => return "image/x-icon",
            "tiff" | "tif" => return "image/tiff",
            "pdf" => return "application/pdf",
            _ => return "text/plain; charset=utf-8",
        }
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
    } else if bytes.starts_with(b"BM") {
        "image/bmp"
    } else if bytes.starts_with(b"<svg") {
        "image/svg+xml"
    } else {
        "text/plain; charset=utf-8"
    }
}

pub fn is_image(bytes: &[u8], ext: Option<&str>) -> bool {
    is_image_ext(ext) || detect_mime(bytes, None).starts_with("image/")
}

pub fn is_pdf(bytes: &[u8], ext: Option<&str>) -> bool {
    is_pdf_ext(ext) || detect_mime(bytes, None) == "application/pdf"
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
    fn test_detect_mime_non_executable() {
        // Executable or rich text formats must never return executable MIME types
        assert_eq!(detect_mime(b"alert(1)", Some("html")), "text/plain; charset=utf-8");
        assert_eq!(detect_mime(b"alert(1)", Some("js")), "text/plain; charset=utf-8");
        assert_eq!(detect_mime(b"<xml></xml>", Some("xml")), "text/plain; charset=utf-8");
        assert_eq!(detect_mime(b"body {}", Some("css")), "text/plain; charset=utf-8");
        assert_eq!(detect_mime(b"{}", Some("json")), "text/plain; charset=utf-8");
        assert_eq!(detect_mime(b"fn main() {}", Some("rs")), "text/plain; charset=utf-8");
        assert_eq!(detect_mime(b"print(1)", Some("py")), "text/plain; charset=utf-8");

        // Media extensions return proper media MIME
        assert_eq!(detect_mime(b"", Some("png")), "image/png");
        assert_eq!(detect_mime(b"", Some("jpg")), "image/jpeg");
        assert_eq!(detect_mime(b"", Some("jpeg")), "image/jpeg");
        assert_eq!(detect_mime(b"", Some("gif")), "image/gif");
        assert_eq!(detect_mime(b"", Some("webp")), "image/webp");
        assert_eq!(detect_mime(b"", Some("svg")), "image/svg+xml");
        assert_eq!(detect_mime(b"", Some("avif")), "image/avif");
        assert_eq!(detect_mime(b"", Some("bmp")), "image/bmp");
        assert_eq!(detect_mime(b"", Some("ico")), "image/x-icon");
        assert_eq!(detect_mime(b"", Some("tiff")), "image/tiff");
        assert_eq!(detect_mime(b"", Some("tif")), "image/tiff");
        assert_eq!(detect_mime(b"", Some("pdf")), "application/pdf");
    }

    #[test]
    fn test_detect_mime_svg_sniffing() {
        // Content merely mentioning <svg later in the body must NOT be classified as svg
        let html_content = b"<!DOCTYPE html><html><body><div class=\"icon\"><svg viewBox=\"0 0 10 10\"></svg></div></body></html>";
        assert_eq!(detect_mime(html_content, None), "text/plain; charset=utf-8");

        // Starts with <svg without ext is classified as image/svg+xml
        let svg_content = b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>";
        assert_eq!(detect_mime(svg_content, None), "image/svg+xml");
    }
}
