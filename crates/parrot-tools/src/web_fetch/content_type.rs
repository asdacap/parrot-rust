// MediaTypeHeaderValue permits OWS, bare parameters, and quoted-pairs that
// mime::Mime does not. Validate the complete source-shaped header before using
// the MIME crate for its media type; never discard malformed parameter suffixes.
pub(super) fn media_type(input: &str) -> Option<String> {
    let mut parser = Parser {
        input: input.as_bytes(),
        position: 0,
    };
    parser.whitespace();
    let kind = parser.token()?;
    parser.whitespace();
    parser.take(b'/')?;
    parser.whitespace();
    let subtype = parser.token()?;
    parser.whitespace();
    while parser.position < parser.input.len() {
        parser.take(b';')?;
        parser.whitespace();
        parser.token()?;
        parser.whitespace();
        if parser.input.get(parser.position) == Some(&b'=') {
            parser.position += 1;
            parser.whitespace();
            if parser.input.get(parser.position) == Some(&b'"') {
                parser.quoted()?;
            } else {
                parser.token()?;
            }
            parser.whitespace();
        }
    }
    Some(format!("{kind}/{subtype}"))
}

struct Parser<'a> {
    input: &'a [u8],
    position: usize,
}

impl<'a> Parser<'a> {
    fn whitespace(&mut self) {
        while self
            .input
            .get(self.position)
            .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
        {
            self.position += 1;
        }
    }

    fn token(&mut self) -> Option<&'a str> {
        let start = self.position;
        while self.input.get(self.position).is_some_and(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        }) {
            self.position += 1;
        }
        (self.position > start)
            .then(|| std::str::from_utf8(&self.input[start..self.position]).expect("ASCII token"))
    }

    fn take(&mut self, byte: u8) -> Option<()> {
        if self.input.get(self.position) != Some(&byte) {
            return None;
        }
        self.position += 1;
        Some(())
    }

    fn quoted(&mut self) -> Option<()> {
        self.take(b'"')?;
        while let Some(&byte) = self.input.get(self.position) {
            self.position += 1;
            match byte {
                b'"' => return Some(()),
                0 | b'\r' | b'\n' => return None,
                b'\\' => {
                    let escaped = *self.input.get(self.position)?;
                    if matches!(escaped, 0 | b'\r' | b'\n') {
                        return None;
                    }
                    // The source BCL accepts an escaped quote only at the exact
                    // input end as its terminator; even trailing OWS changes this.
                    if escaped == b'"' && self.position + 1 == self.input.len() {
                        self.position += 1;
                        return Some(());
                    }
                    self.position += 1;
                }
                _ => {}
            }
        }
        None
    }
}
