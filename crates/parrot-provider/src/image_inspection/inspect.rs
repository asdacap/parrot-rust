use std::fmt;

pub const HEADER_LENGTH: usize = 12;

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
const JPEG_SIGNATURE: &[u8] = b"\xff\xd8\xff";
const INVALID: &str = "The image content is invalid or unsupported.";
const TRUNCATED: &str = "The image content is truncated or invalid.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageInspection {
    pub media_type: &'static str,
    pub width: i32,
    pub height: i32,
    pub frame_count: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageInspectionError(&'static str);

impl fmt::Display for ImageInspectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for ImageInspectionError {}

type Result<T> = std::result::Result<T, ImageInspectionError>;

pub fn detect_media_type(header: &[u8]) -> Option<&'static str> {
    if header.starts_with(PNG_SIGNATURE) {
        Some("image/png")
    } else if header.starts_with(JPEG_SIGNATURE) {
        Some("image/jpeg")
    } else if header.starts_with(b"GIF87a") || header.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if header.len() >= HEADER_LENGTH
        && header.starts_with(b"RIFF")
        && &header[8..12] == b"WEBP"
    {
        Some("image/webp")
    } else {
        None
    }
}

pub fn inspect(data: &[u8]) -> Result<ImageInspection> {
    match detect_media_type(data) {
        Some("image/png") => inspect_png(data),
        Some("image/jpeg") => inspect_jpeg(data),
        Some("image/gif") => inspect_gif(data),
        Some("image/webp") => inspect_webp(data),
        _ => Err(ImageInspectionError(INVALID)),
    }
}

fn create(
    media_type: &'static str,
    width: i32,
    height: i32,
    frame_count: i32,
) -> Result<ImageInspection> {
    if width > 0 && height > 0 && frame_count > 0 {
        Ok(ImageInspection {
            media_type,
            width,
            height,
            frame_count,
        })
    } else {
        Err(ImageInspectionError(INVALID))
    }
}

fn read(data: &[u8], offset: u64, length: u64) -> Result<&[u8]> {
    let end = offset
        .checked_add(length)
        .ok_or(ImageInspectionError(TRUNCATED))?;
    if end > data.len() as u64 {
        return Err(ImageInspectionError(TRUNCATED));
    }
    Ok(&data[offset as usize..end as usize])
}

fn be_u16(data: &[u8], offset: u64) -> Result<u16> {
    let bytes = read(data, offset, 2)?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn le_u16(data: &[u8], offset: u64) -> Result<u16> {
    let bytes = read(data, offset, 2)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn be_u32(data: &[u8], offset: u64) -> Result<u32> {
    let bytes = read(data, offset, 4)?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn le_u32(data: &[u8], offset: u64) -> Result<u32> {
    let bytes = read(data, offset, 4)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn inspect_png(data: &[u8]) -> Result<ImageInspection> {
    let mut width = 0;
    let mut height = 0;
    let mut frame_count = 1;
    let mut has_image_data = false;
    let mut offset = PNG_SIGNATURE.len() as u64;
    loop {
        let length = u64::from(be_u32(data, offset)?);
        let chunk_type = read(data, offset + 4, 4)?;
        let body = read(data, offset + 8, length)?;
        read(data, offset + 8 + length, 4)?;
        let is_header = chunk_type == b"IHDR";
        if (offset == PNG_SIGNATURE.len() as u64) != is_header {
            return Err(ImageInspectionError(
                "A PNG image must start with exactly one IHDR chunk.",
            ));
        }
        match chunk_type {
            b"IHDR" => {
                width = be_u32(body, 0)? as i32;
                height = be_u32(body, 4)? as i32;
            }
            b"acTL" => frame_count = be_u32(body, 0)? as i32,
            b"IDAT" => has_image_data = true,
            b"IEND" => {
                return create(
                    "image/png",
                    width,
                    height,
                    if has_image_data { frame_count } else { 0 },
                );
            }
            _ => {}
        }
        offset += 12 + length;
    }
}

fn inspect_jpeg(data: &[u8]) -> Result<ImageInspection> {
    let mut width = 0;
    let mut height = 0;
    let mut offset = 2;
    loop {
        let marker = read(data, offset, 2)?;
        if marker[0] != 0xff {
            return Err(ImageInspectionError(
                "The JPEG image contains an invalid marker.",
            ));
        }
        match marker[1] {
            0xff => {
                offset += 1;
                continue;
            }
            0xd9 => return create("image/jpeg", width, height, 1),
            0x01 | 0xd0..=0xd7 => {
                offset += 2;
                continue;
            }
            _ => {}
        }
        let length = u64::from(be_u16(data, offset + 2)?);
        let body_length = length
            .checked_sub(2)
            .ok_or(ImageInspectionError(TRUNCATED))?;
        let segment = read(data, offset + 4, body_length)?;
        if (0xc0..=0xcf).contains(&marker[1]) && !matches!(marker[1], 0xc4 | 0xc8 | 0xcc) {
            height = i32::from(be_u16(segment, 1)?);
            width = i32::from(be_u16(segment, 3)?);
        }
        offset += 2 + length;
        if marker[1] == 0xda {
            offset = skip_jpeg_entropy_coded_data(data, offset)?;
        }
    }
}

fn skip_jpeg_entropy_coded_data(data: &[u8], mut offset: u64) -> Result<u64> {
    loop {
        let remaining = (data.len() as u64)
            .checked_sub(offset)
            .ok_or(ImageInspectionError(TRUNCATED))?;
        let marker_index = read(data, offset, remaining)?
            .iter()
            .position(|&byte| byte == 0xff)
            .ok_or(ImageInspectionError("The JPEG image is truncated."))?;
        offset += marker_index as u64;
        if !matches!(read(data, offset + 1, 1)?[0], 0x00 | 0xd0..=0xd7) {
            return Ok(offset);
        }
        offset += 2;
    }
}

fn inspect_gif(data: &[u8]) -> Result<ImageInspection> {
    let screen = read(data, 6, 7)?;
    let width = i32::from(le_u16(screen, 0)?);
    let height = i32::from(le_u16(screen, 2)?);
    let mut frame_count: i32 = 0;
    let mut offset = 13 + gif_color_table_length(screen[4]);
    loop {
        match read(data, offset, 1)?[0] {
            0x3b => return create("image/gif", width, height, frame_count),
            0x21 => offset = skip_gif_sub_blocks(data, offset + 2)?,
            0x2c => {
                let descriptor = read(data, offset + 1, 9)?;
                offset =
                    skip_gif_sub_blocks(data, offset + 11 + gif_color_table_length(descriptor[8]))?;
                frame_count = frame_count
                    .checked_add(1)
                    .ok_or(ImageInspectionError(INVALID))?;
            }
            _ => {
                return Err(ImageInspectionError(
                    "The GIF image contains an unknown block.",
                ));
            }
        }
    }
}

fn gif_color_table_length(flags: u8) -> u64 {
    if flags & 0x80 == 0 {
        0
    } else {
        3 << ((flags & 0x07) + 1)
    }
}

fn skip_gif_sub_blocks(data: &[u8], mut offset: u64) -> Result<u64> {
    loop {
        let size = read(data, offset, 1)?[0];
        offset += 1 + u64::from(size);
        if size == 0 {
            return Ok(offset);
        }
    }
}

fn inspect_webp(data: &[u8]) -> Result<ImageInspection> {
    let riff = read(data, 0, 8 + u64::from(le_u32(data, 4)?))?;
    let mut width = 0;
    let mut height = 0;
    let mut animated = false;
    let mut has_image_data = false;
    let mut animation_frames: i32 = 0;
    let mut offset = HEADER_LENGTH as u64;
    while offset < riff.len() as u64 {
        let chunk_type = read(riff, offset, 4)?;
        let size = u64::from(le_u32(riff, offset + 4)?);
        let body = read(riff, offset + 8, size)?;
        match chunk_type {
            b"VP8X" => {
                let canvas = read(body, 0, 10)?;
                animated = canvas[0] & 0x02 != 0;
                width = 1
                    + i32::from(canvas[4])
                    + (i32::from(canvas[5]) << 8)
                    + (i32::from(canvas[6]) << 16);
                height = 1
                    + i32::from(canvas[7])
                    + (i32::from(canvas[8]) << 8)
                    + (i32::from(canvas[9]) << 16);
            }
            b"VP8 " => {
                if read(body, 3, 3)? != b"\x9d\x01\x2a" {
                    return Err(ImageInspectionError(
                        "The WebP image contains an invalid VP8 frame.",
                    ));
                }
                has_image_data = true;
                if width == 0 {
                    width = i32::from(le_u16(body, 6)? & 0x3fff);
                }
                if height == 0 {
                    height = i32::from(le_u16(body, 8)? & 0x3fff);
                }
            }
            b"VP8L" => {
                if read(body, 0, 1)?[0] != 0x2f {
                    return Err(ImageInspectionError(
                        "The WebP image contains an invalid VP8L frame.",
                    ));
                }
                let bits = le_u32(body, 1)?;
                has_image_data = true;
                if width == 0 {
                    width = (bits & 0x3fff) as i32 + 1;
                }
                if height == 0 {
                    height = ((bits >> 14) & 0x3fff) as i32 + 1;
                }
            }
            b"ANMF" => {
                animation_frames = animation_frames
                    .checked_add(1)
                    .ok_or(ImageInspectionError(INVALID))?
            }
            _ => {}
        }
        offset += 8 + size + (size & 1);
    }
    create(
        "image/webp",
        width,
        height,
        if animated {
            animation_frames
        } else if has_image_data {
            1
        } else {
            0
        },
    )
}
