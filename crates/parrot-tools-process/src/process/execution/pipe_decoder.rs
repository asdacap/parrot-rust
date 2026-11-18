use super::output::Utf8Decoder;

#[derive(Clone, Copy)]
enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Utf32Le,
    Utf32Be,
}

const MARKERS: &[(&[u8], Encoding)] = &[
    (&[0xff, 0xfe, 0, 0], Encoding::Utf32Le),
    (&[0, 0, 0xfe, 0xff], Encoding::Utf32Be),
    (&[0xef, 0xbb, 0xbf], Encoding::Utf8),
    (&[0xff, 0xfe], Encoding::Utf16Le),
    (&[0xfe, 0xff], Encoding::Utf16Be),
];

/// StreamReader-style BOM detection and replacement decoding for redirected pipes.
/// Only a partial marker/code unit and an unmatched high surrogate span reads.
#[derive(Default)]
pub(super) struct PipeDecoder {
    encoding: Option<Encoding>,
    pending: Vec<u8>,
    utf8: Utf8Decoder,
    high: Option<u16>,
}

impl PipeDecoder {
    pub(super) fn append(&mut self, bytes: &[u8], complete: bool) -> Vec<u16> {
        self.pending.extend_from_slice(bytes);
        let encoding = match self.encoding {
            Some(encoding) => encoding,
            None => {
                if !complete
                    && MARKERS.iter().any(|(marker, _)| {
                        marker.len() > self.pending.len() && marker.starts_with(&self.pending)
                    })
                {
                    return Vec::new();
                }
                let (encoding, length) = MARKERS
                    .iter()
                    .find_map(|(marker, encoding)| {
                        self.pending
                            .starts_with(marker)
                            .then_some((*encoding, marker.len()))
                    })
                    .unwrap_or((Encoding::Utf8, 0));
                self.pending.drain(..length);
                self.encoding = Some(encoding);
                encoding
            }
        };
        if matches!(encoding, Encoding::Utf8) {
            let units = self.utf8.append(&self.pending, complete);
            self.pending.clear();
            return units;
        }
        let width = match encoding {
            Encoding::Utf16Le | Encoding::Utf16Be => 2,
            Encoding::Utf32Le | Encoding::Utf32Be => 4,
            Encoding::Utf8 => unreachable!(),
        };
        let mut units = Vec::new();
        let mut consumed = 0;
        while consumed + width <= self.pending.len() {
            let chunk = &self.pending[consumed..consumed + width];
            match encoding {
                Encoding::Utf16Le | Encoding::Utf16Be => {
                    let pair = [chunk[0], chunk[1]];
                    let unit = if matches!(encoding, Encoding::Utf16Le) {
                        u16::from_le_bytes(pair)
                    } else {
                        u16::from_be_bytes(pair)
                    };
                    if let Some(high) = self.high.take() {
                        if (0xdc00..=0xdfff).contains(&unit) {
                            units.extend_from_slice(&[high, unit]);
                            consumed += width;
                            continue;
                        }
                        units.push(0xfffd);
                    }
                    match unit {
                        0xd800..=0xdbff => self.high = Some(unit),
                        0xdc00..=0xdfff => units.push(0xfffd),
                        _ => units.push(unit),
                    }
                }
                Encoding::Utf32Le | Encoding::Utf32Be => {
                    let word = [chunk[0], chunk[1], chunk[2], chunk[3]];
                    let scalar = if matches!(encoding, Encoding::Utf32Le) {
                        u32::from_le_bytes(word)
                    } else {
                        u32::from_be_bytes(word)
                    };
                    let character = char::from_u32(scalar).unwrap_or(char::REPLACEMENT_CHARACTER);
                    units.extend_from_slice(character.encode_utf16(&mut [0; 2]));
                }
                Encoding::Utf8 => unreachable!(),
            }
            consumed += width;
        }
        self.pending.drain(..consumed);
        if complete {
            if self.high.take().is_some() {
                units.push(0xfffd);
            }
            if !self.pending.is_empty() {
                units.push(0xfffd);
                self.pending.clear();
            }
        }
        units
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(text: &str, marker: &[u8], width: usize, little: bool) -> Vec<u8> {
        let mut bytes = marker.to_vec();
        if width == 2 {
            for unit in text.encode_utf16() {
                bytes.extend(if little {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                });
            }
        } else {
            for character in text.chars() {
                let scalar = character as u32;
                bytes.extend(if little {
                    scalar.to_le_bytes()
                } else {
                    scalar.to_be_bytes()
                });
            }
        }
        bytes
    }

    #[test]
    fn supported_encodings_preserve_text_at_every_byte_boundary() {
        let text = "A€😀\0\r\nZ";
        let cases = [
            text.as_bytes().to_vec(),
            [b"\xef\xbb\xbf".as_slice(), text.as_bytes()].concat(),
            encoded(text, &[0xff, 0xfe], 2, true),
            encoded(text, &[0xfe, 0xff], 2, false),
            encoded(text, &[0xff, 0xfe, 0, 0], 4, true),
            encoded(text, &[0, 0, 0xfe, 0xff], 4, false),
        ];
        let expected = text.encode_utf16().collect::<Vec<_>>();
        for bytes in cases {
            for split in 0..=bytes.len() {
                let mut decoder = PipeDecoder::default();
                let mut units = decoder.append(&bytes[..split], false);
                units.extend(decoder.append(&bytes[split..], false));
                units.extend(decoder.append(&[], true));
                assert_eq!(units, expected, "bytes={bytes:?}, split={split}");
                assert!(decoder.append(&[], true).is_empty());
            }
            for width in 1..=7 {
                let mut decoder = PipeDecoder::default();
                let mut units = Vec::new();
                for chunk in bytes.chunks(width) {
                    units.extend(decoder.append(chunk, false));
                    assert!(decoder.pending.len() <= 3);
                }
                units.extend(decoder.append(&[], true));
                assert_eq!(units, expected, "bytes={bytes:?}, chunk={width}");
            }
        }
    }

    #[test]
    fn invalid_and_truncated_units_use_replacement_without_losing_neighbors() {
        let cases: &[(&[u8], &str)] = &[
            (&[0xff], "�"),
            (&[0xef, 0xbb], "�"),
            (&[0, 0, 0xfe], "\0\0�"),
            (&[0xff, 0xfe, 0], "�"),
            (&[0xff, 0xfe, 0, 0xd8, 0x41, 0, 0, 0xdc], "�A�"),
            (&[0xff, 0xfe, 0, 0xd8, 0xff], "��"),
            (&[0xfe, 0xff, 0xd8, 0, 0, 0x41, 0xdc, 0], "�A�"),
            (&[0xff, 0xfe, 0, 0, 0, 0xd8, 0, 0], "�"),
            (&[0, 0, 0xfe, 0xff, 0, 0x11, 0, 0, 0], "��"),
            (&[0xef, 0xbb, 0xbf, 0xf0, 0x80, 0x80, 0x80], "����"),
        ];
        for (bytes, expected) in cases {
            for width in 1..=bytes.len() {
                let mut decoder = PipeDecoder::default();
                let mut units = Vec::new();
                for chunk in bytes.chunks(width) {
                    units.extend(decoder.append(chunk, false));
                }
                units.extend(decoder.append(&[], true));
                assert_eq!(
                    String::from_utf16(&units).unwrap(),
                    *expected,
                    "{bytes:?}/{width}"
                );
            }
        }
    }

    #[test]
    fn only_the_initial_marker_is_removed_and_plain_utf8_is_not_delayed() {
        let mut decoder = PipeDecoder::default();
        assert_eq!(decoder.append(b"A", false), vec![u16::from(b'A')]);
        assert_eq!(decoder.append(b"\xef\xbb\xbf", false), vec![0xfeff]);
        let mut decoder = PipeDecoder::default();
        assert_eq!(
            decoder.append(b"\xef\xbb\xbf\xef\xbb\xbfA", true),
            vec![0xfeff, u16::from(b'A')]
        );
        let mut decoder = PipeDecoder::default();
        assert_eq!(decoder.append(b"A\0", true), vec![u16::from(b'A'), 0]);
        let mut pty = Utf8Decoder::default();
        assert_eq!(
            pty.append(b"\xef\xbb\xbfA", true),
            vec![0xfeff, u16::from(b'A')]
        );
    }
}
