//! Signature recognition for the image decoders in the pinned private engine.
//! This module does not decode or validate image pixels.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Bmp,
    Gif,
    Psd,
    Hdr,
    Pic,
    Pnm,
    Tga,
}

impl ImageFormat {
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Bmp => "image/bmp",
            Self::Gif => "image/gif",
            Self::Psd => "image/vnd.adobe.photoshop",
            Self::Hdr => "image/vnd.radiance",
            Self::Pic => "image/x-softimage",
            Self::Pnm => "image/x-portable-anymap",
            Self::Tga => "image/x-tga",
        }
    }
}

pub fn detect_format(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(ImageFormat::Png);
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some(ImageFormat::Jpeg);
    }
    if bytes.starts_with(b"BM") {
        return Some(ImageFormat::Bmp);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(ImageFormat::Gif);
    }
    if bytes.starts_with(b"8BPS\0\x01") {
        return Some(ImageFormat::Psd);
    }
    if bytes.starts_with(b"#?RADIANCE\n") || bytes.starts_with(b"#?RGBE\n") {
        return Some(ImageFormat::Hdr);
    }
    if bytes.starts_with(b"\x53\x80\xf6\x34") && bytes.get(88..92) == Some(b"PICT") {
        return Some(ImageFormat::Pic);
    }
    if bytes.starts_with(b"P5") || bytes.starts_with(b"P6") {
        return Some(ImageFormat::Pnm);
    }
    if bytes.len() >= 26 && bytes.ends_with(b"TRUEVISION-XFILE.\0") {
        return Some(ImageFormat::Tga);
    }
    let header = bytes.get(..18)?;
    let palette = header[1];
    let image_type = header[2];
    let bpp = header[16];
    let supported = match palette {
        0 => matches!(image_type, 2 | 3 | 10 | 11),
        1 => {
            matches!(image_type, 1 | 9)
                && matches!(header[7], 8 | 15 | 16 | 24 | 32)
                && matches!(bpp, 8 | 16)
        }
        _ => false,
    };
    if supported
        && matches!(bpp, 8 | 15 | 16 | 24 | 32)
        && u16::from_le_bytes([header[12], header[13]]) > 0
        && u16::from_le_bytes([header[14], header[15]]) > 0
    {
        Some(ImageFormat::Tga)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_textual_image_headers_before_text_and_excludes_webp() {
        assert_eq!(detect_format(b"P6\n1 1\n255\nabc"), Some(ImageFormat::Pnm));
        assert_eq!(
            detect_format(b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n"),
            Some(ImageFormat::Hdr)
        );
        assert_eq!(detect_format(b"RIFF\x00\x00\x00\x00WEBP"), None);
        assert_eq!(detect_format(b"plain text"), None);
        assert_eq!(detect_format(&[0; 18]), None);
        let mut header = [0u8; 18];
        header[2] = 2;
        header[12] = 1;
        header[14] = 1;
        header[16] = 24;
        assert_eq!(detect_format(&header), Some(ImageFormat::Tga));
        header[16] = 1;
        assert_eq!(detect_format(&header), None);
    }
}
