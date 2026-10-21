use std::fmt;

use super::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageInspection {
    pub media_type: &'static str,
    pub width: u32,
    pub height: u32,
    pub frame_count: u32,
}

impl ImageInspection {
    pub const HEADER_LENGTH: usize = 12;

    pub fn detect_media_type(header: &[u8]) -> Option<&'static str> {
        if header.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some("image/png")
        } else if header.starts_with(b"\xff\xd8\xff") {
            Some("image/jpeg")
        } else if header.starts_with(b"GIF87a") || header.starts_with(b"GIF89a") {
            Some("image/gif")
        } else if header.len() >= Self::HEADER_LENGTH
            && header.starts_with(b"RIFF")
            && header[8..12] == *b"WEBP"
        {
            Some("image/webp")
        } else {
            None
        }
    }

    pub fn inspect(data: &[u8]) -> Result<Self, Error> {
        match Self::detect_media_type(data) {
            Some("image/png") => Self::inspect_png(data),
            Some("image/jpeg") => Self::inspect_jpeg(data),
            Some("image/gif") => Self::inspect_gif(data),
            Some("image/webp") => Self::inspect_webp(data),
            _ => Err(Error::invalid_data(
                "The image content is invalid or unsupported.",
            )),
        }
    }

    fn create(
        media_type: &'static str,
        width: u32,
        height: u32,
        frames: u32,
    ) -> Result<Self, Error> {
        if width == 0 || height == 0 || frames == 0 {
            return Err(Error::invalid_data(
                "The image content is invalid or unsupported.",
            ));
        }
        Ok(Self {
            media_type,
            width,
            height,
            frame_count: frames,
        })
    }

    fn read(data: &[u8], offset: usize, len: usize) -> Result<&[u8], Error> {
        let end = offset.checked_add(len).ok_or_else(Self::truncated)?;
        data.get(offset..end).ok_or_else(Self::truncated)
    }

    fn truncated() -> Error {
        Error::invalid_data("The image content is truncated or invalid.")
    }

    fn inspect_png(data: &[u8]) -> Result<Self, Error> {
        let mut width = 0;
        let mut height = 0;
        let mut frames = 1i32;
        let mut has_data = false;
        let mut offset = 8usize;
        loop {
            let len = u32::from_be_bytes(
                Self::read(data, offset, 4)?
                    .try_into()
                    .map_err(|_| Self::truncated())?,
            ) as usize;
            let kind = Self::read(data, offset + 4, 4)?;
            let body = Self::read(data, offset + 8, len)?;
            let _crc = Self::read(data, offset + 8 + len, 4)?;
            let is_header = kind == b"IHDR";
            if (offset == 8) != is_header {
                return Err(Error::invalid_data(
                    "A PNG image must start with exactly one IHDR chunk.",
                ));
            }
            if is_header {
                let png_width = i32::from_be_bytes(
                    Self::read(body, 0, 4)?
                        .try_into()
                        .map_err(|_| Self::truncated())?,
                );
                let png_height = i32::from_be_bytes(
                    Self::read(body, 4, 4)?
                        .try_into()
                        .map_err(|_| Self::truncated())?,
                );
                if png_width <= 0 || png_height <= 0 {
                    return Err(Error::invalid_data(
                        "The image content is invalid or unsupported.",
                    ));
                }
                width = png_width as u32;
                height = png_height as u32;
            } else if kind == b"acTL" {
                frames = i32::from_be_bytes(
                    Self::read(body, 0, 4)?
                        .try_into()
                        .map_err(|_| Self::truncated())?,
                );
            } else if kind == b"IDAT" {
                has_data = true;
            } else if kind == b"IEND" {
                return Self::create(
                    "image/png",
                    width,
                    height,
                    if has_data {
                        u32::try_from(frames).unwrap_or(0)
                    } else {
                        0
                    },
                );
            }
            offset = offset
                .checked_add(12)
                .and_then(|n| n.checked_add(len))
                .ok_or_else(Self::truncated)?;
        }
    }

    fn inspect_jpeg(data: &[u8]) -> Result<Self, Error> {
        let (mut width, mut height, mut offset) = (0u32, 0u32, 2usize);
        loop {
            let marker = Self::read(data, offset, 2)?;
            if marker[0] != 0xff {
                return Err(Error::invalid_data(
                    "The JPEG image contains an invalid marker.",
                ));
            }
            if marker[1] == 0xd9 {
                return Self::create("image/jpeg", width, height, 1);
            }
            if marker[1] == 0xff {
                offset += 1;
                continue;
            }
            if marker[1] == 0x01 || (0xd0..=0xd7).contains(&marker[1]) {
                offset += 2;
                continue;
            }
            let len = u16::from_be_bytes(
                Self::read(data, offset + 2, 2)?
                    .try_into()
                    .map_err(|_| Self::truncated())?,
            ) as usize;
            if len < 2 {
                return Err(Self::truncated());
            }
            let segment = Self::read(data, offset + 4, len - 2)?;
            if (0xc0..=0xcf).contains(&marker[1]) && ![0xc4, 0xc8, 0xcc].contains(&marker[1]) {
                height = u16::from_be_bytes(
                    Self::read(segment, 1, 2)?
                        .try_into()
                        .map_err(|_| Self::truncated())?,
                ) as u32;
                width = u16::from_be_bytes(
                    Self::read(segment, 3, 2)?
                        .try_into()
                        .map_err(|_| Self::truncated())?,
                ) as u32;
            }
            offset = offset.checked_add(2 + len).ok_or_else(Self::truncated)?;
            if marker[1] == 0xda {
                offset = Self::skip_jpeg_entropy(data, offset)?;
            }
        }
    }

    fn skip_jpeg_entropy(data: &[u8], mut offset: usize) -> Result<usize, Error> {
        loop {
            let slice = data.get(offset..).ok_or_else(Self::truncated)?;
            let marker = slice
                .iter()
                .position(|b| *b == 0xff)
                .ok_or_else(|| Error::invalid_data("The JPEG image is truncated."))?;
            offset += marker;
            let next = *Self::read(data, offset + 1, 1)?
                .first()
                .ok_or_else(Self::truncated)?;
            if next != 0x00 && !(0xd0..=0xd7).contains(&next) {
                return Ok(offset);
            }
            offset += 2;
        }
    }

    fn inspect_gif(data: &[u8]) -> Result<Self, Error> {
        let screen = Self::read(data, 6, 7)?;
        let width = u16::from_le_bytes([screen[0], screen[1]]) as u32;
        let height = u16::from_le_bytes([screen[2], screen[3]]) as u32;
        let mut frames = 0;
        let mut offset = 13usize + Self::gif_color_table_length(screen[4]);
        loop {
            match Self::read(data, offset, 1)?[0] {
                0x3b => return Self::create("image/gif", width, height, frames),
                0x21 => offset = Self::skip_gif_subblocks(data, offset + 2)?,
                0x2c => {
                    let descriptor = Self::read(data, offset + 1, 9)?;
                    offset = Self::skip_gif_subblocks(
                        data,
                        offset + 11 + Self::gif_color_table_length(descriptor[8]),
                    )?;
                    frames += 1;
                }
                _ => {
                    return Err(Error::invalid_data(
                        "The GIF image contains an unknown block.",
                    ));
                }
            }
        }
    }

    fn gif_color_table_length(flags: u8) -> usize {
        if flags & 0x80 == 0 {
            0
        } else {
            3usize << ((flags & 0x07) + 1)
        }
    }

    fn skip_gif_subblocks(data: &[u8], mut offset: usize) -> Result<usize, Error> {
        loop {
            let size = Self::read(data, offset, 1)?[0] as usize;
            offset = offset.checked_add(1 + size).ok_or_else(Self::truncated)?;
            if size == 0 {
                return Ok(offset);
            }
            let _ = Self::read(data, offset - size, size)?;
        }
    }

    fn inspect_webp(data: &[u8]) -> Result<Self, Error> {
        let riff_len = u32::from_le_bytes(
            Self::read(data, 4, 4)?
                .try_into()
                .map_err(|_| Self::truncated())?,
        ) as usize;
        let riff_end = 8usize.checked_add(riff_len).ok_or_else(Self::truncated)?;
        let riff = Self::read(data, 0, riff_end)?;
        let (mut width, mut height, mut animated, mut has_data, mut frames) =
            (0u32, 0u32, false, false, 0u32);
        let mut offset = 12usize;
        while offset < riff.len() {
            let kind = Self::read(riff, offset, 4)?;
            let len = u32::from_le_bytes(
                Self::read(riff, offset + 4, 4)?
                    .try_into()
                    .map_err(|_| Self::truncated())?,
            ) as usize;
            let body = Self::read(riff, offset + 8, len)?;
            if kind == b"VP8X" {
                let canvas = Self::read(body, 0, 10)?;
                animated = canvas[0] & 0x02 != 0;
                width =
                    1 + (canvas[4] as u32 | ((canvas[5] as u32) << 8) | ((canvas[6] as u32) << 16));
                height =
                    1 + (canvas[7] as u32 | ((canvas[8] as u32) << 8) | ((canvas[9] as u32) << 16));
            } else if kind == b"VP8 " {
                if Self::read(body, 3, 3)? != [0x9d, 0x01, 0x2a] {
                    return Err(Error::invalid_data(
                        "The WebP image contains an invalid VP8 frame.",
                    ));
                }
                has_data = true;
                if width == 0 {
                    width = u16::from_le_bytes(
                        Self::read(body, 6, 2)?
                            .try_into()
                            .map_err(|_| Self::truncated())?,
                    ) as u32
                        & 0x3fff;
                }
                if height == 0 {
                    height = u16::from_le_bytes(
                        Self::read(body, 8, 2)?
                            .try_into()
                            .map_err(|_| Self::truncated())?,
                    ) as u32
                        & 0x3fff;
                }
            } else if kind == b"VP8L" {
                if Self::read(body, 0, 1)?[0] != 0x2f {
                    return Err(Error::invalid_data(
                        "The WebP image contains an invalid VP8L frame.",
                    ));
                }
                let bits = u32::from_le_bytes(
                    Self::read(body, 1, 4)?
                        .try_into()
                        .map_err(|_| Self::truncated())?,
                );
                has_data = true;
                if width == 0 {
                    width = (bits & 0x3fff) + 1;
                }
                if height == 0 {
                    height = ((bits >> 14) & 0x3fff) + 1;
                }
            } else if kind == b"ANMF" {
                frames += 1;
            }
            offset = offset
                .checked_add(8 + len + (len & 1))
                .ok_or_else(Self::truncated)?;
        }
        Self::create(
            "image/webp",
            width,
            height,
            if animated {
                frames
            } else if has_data {
                1
            } else {
                0
            },
        )
    }
}

impl fmt::Display for ImageInspection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {}x{} ({})",
            self.media_type, self.width, self.height, self.frame_count
        )
    }
}
