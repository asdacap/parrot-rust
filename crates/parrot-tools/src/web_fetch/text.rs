//! Text handling from the pinned WebFetchText/HtmlText implementation.

pub(crate) fn strip_controls(text: &str) -> String {
    // The caller performs UTF-8 replacement decoding first. The source removes
    // both those replacement runes and literal U+FFFD from the decoded text.
    text.chars()
        .filter(|&ch| matches!(ch, '\n' | '\r' | '\t') || (!ch.is_control() && ch != '\u{fffd}'))
        .collect()
}

pub(crate) fn extract_html(input: &str) -> String {
    let mut output = String::new();
    let mut hidden = String::new();
    let mut position = 0;

    while position < input.len() {
        let rest = &input[position..];
        if !rest.starts_with('<') {
            let next = rest.find('<').unwrap_or(rest.len());
            if hidden.is_empty() {
                output.push_str(&rest[..next]);
            }
            position += next;
            continue;
        }
        if let Some(comment) = rest.strip_prefix("<!--") {
            let Some(end) = comment.find("-->") else {
                break;
            };
            position += 4 + end + 3;
            continue;
        }
        let Some(end) = find_tag_end(rest) else {
            if hidden.is_empty() {
                output.push_str(rest);
            }
            break;
        };
        let (tag, closing) = read_tag(&rest[1..end]);
        if matches!(tag.as_str(), "script" | "style" | "noscript" | "template") {
            if closing && hidden == tag {
                hidden.clear();
            } else if !closing && hidden.is_empty() {
                hidden = tag.clone();
            }
        }
        if hidden.is_empty() && is_block_tag(&tag) {
            output.push('\n');
        }
        position += end + 1;
    }

    normalize(&strip_controls(&html_decode(&output)))
}

fn find_tag_end(input: &str) -> Option<usize> {
    let mut quote = None;
    for (index, byte) in input.bytes().enumerate().skip(1) {
        if let Some(open) = quote {
            if byte == open {
                quote = None;
            }
        } else if matches!(byte, b'\'' | b'"') {
            quote = Some(byte);
        } else if byte == b'>' {
            return Some(index);
        }
    }
    None
}

fn read_tag(raw: &str) -> (String, bool) {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with(['!', '?']) {
        return (String::new(), false);
    }
    let closing = raw.starts_with('/');
    let raw = if closing { raw[1..].trim_start() } else { raw };
    let end = raw
        .char_indices()
        .find(|&(_, ch)| !is_tag_letter_or_digit(ch))
        .map_or(raw.len(), |(index, _)| index);
    (raw[..end].to_lowercase(), closing)
}

fn is_block_tag(tag: &str) -> bool {
    matches!(
        tag,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "br"
            | "div"
            | "dl"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hr"
            | "li"
            | "main"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "section"
            | "table"
            | "tr"
            | "ul"
    )
}

fn normalize(text: &str) -> String {
    let mut lines = Vec::new();
    let mut blank = true;
    // CR and LF are separate separators in the source: CRLF can produce a
    // blank line, rather than being treated as one platform line ending.
    for raw in text.split(['\r', '\n']) {
        let line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            if !blank {
                lines.push(line);
                blank = true;
            }
        } else {
            lines.push(line);
            blank = false;
        }
    }
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

fn html_decode(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        output.push_str(&rest[..amp]);
        rest = &rest[amp + 1..];
        // Another ampersand terminates the search, so an invalid entity cannot
        // swallow a subsequent valid one. Decoded output is never rescanned.
        if let Some(end) = rest.find([';', '&']) {
            if rest.as_bytes()[end] == b';' {
                let entity = &rest[..end];
                let decoded = if let Some(number) = entity.strip_prefix('#') {
                    decode_number(number)
                } else {
                    named_entity(entity)
                };
                if let Some(ch) = decoded {
                    output.push(ch);
                    rest = &rest[end + 1..];
                    continue;
                }
            }
        }
        output.push('&');
    }
    output.push_str(rest);
    output
}

fn decode_number(number: &str) -> Option<char> {
    // WebUtility uses UInt32.TryParse: decimal Integer style permits ASCII
    // whitespace and a leading sign, whereas AllowHexSpecifier does not.
    // Both parsers permit terminating NULs; negative zero is also valid UInt32.
    let value = if let Some(hex) = number.strip_prefix(['x', 'X']) {
        parse_digits(hex.trim_end_matches('\0'), 16)?
    } else {
        let decimal = number
            .trim_end_matches('\0')
            .trim_matches(|ch| matches!(ch, ' ' | '\t' | '\n' | '\r' | '\u{b}' | '\u{c}'));
        let (negative, digits) = if let Some(digits) = decimal.strip_prefix('-') {
            (true, digits)
        } else {
            (false, decimal.strip_prefix('+').unwrap_or(decimal))
        };
        let value = parse_digits(digits, 10)?;
        if negative && value != 0 {
            return None;
        }
        value
    };
    // Invalid scalars (including surrogate code points) are left verbatim.
    char::from_u32(value)
}

fn parse_digits(digits: &str, radix: u32) -> Option<u32> {
    if digits.is_empty()
        || !digits.bytes().all(|byte| match radix {
            16 => byte.is_ascii_hexdigit(),
            _ => byte.is_ascii_digit(),
        })
    {
        return None;
    }
    u32::from_str_radix(digits, radix).ok()
}

// WebUtility.HtmlDecode's case-sensitive HTML4 table, including its XML apos
// extension. HTML5-only names and names without a semicolon remain unchanged.
fn named_entity(entity: &str) -> Option<char> {
    let value = match entity {
        "quot" => 34,
        "amp" => 38,
        "apos" => 39,
        "lt" => 60,
        "gt" => 62,
        "nbsp" => 160,
        "iexcl" => 161,
        "cent" => 162,
        "pound" => 163,
        "curren" => 164,
        "yen" => 165,
        "brvbar" => 166,
        "sect" => 167,
        "uml" => 168,
        "copy" => 169,
        "ordf" => 170,
        "laquo" => 171,
        "not" => 172,
        "shy" => 173,
        "reg" => 174,
        "macr" => 175,
        "deg" => 176,
        "plusmn" => 177,
        "sup2" => 178,
        "sup3" => 179,
        "acute" => 180,
        "micro" => 181,
        "para" => 182,
        "middot" => 183,
        "cedil" => 184,
        "sup1" => 185,
        "ordm" => 186,
        "raquo" => 187,
        "frac14" => 188,
        "frac12" => 189,
        "frac34" => 190,
        "iquest" => 191,
        "Agrave" => 192,
        "Aacute" => 193,
        "Acirc" => 194,
        "Atilde" => 195,
        "Auml" => 196,
        "Aring" => 197,
        "AElig" => 198,
        "Ccedil" => 199,
        "Egrave" => 200,
        "Eacute" => 201,
        "Ecirc" => 202,
        "Euml" => 203,
        "Igrave" => 204,
        "Iacute" => 205,
        "Icirc" => 206,
        "Iuml" => 207,
        "ETH" => 208,
        "Ntilde" => 209,
        "Ograve" => 210,
        "Oacute" => 211,
        "Ocirc" => 212,
        "Otilde" => 213,
        "Ouml" => 214,
        "times" => 215,
        "Oslash" => 216,
        "Ugrave" => 217,
        "Uacute" => 218,
        "Ucirc" => 219,
        "Uuml" => 220,
        "Yacute" => 221,
        "THORN" => 222,
        "szlig" => 223,
        "agrave" => 224,
        "aacute" => 225,
        "acirc" => 226,
        "atilde" => 227,
        "auml" => 228,
        "aring" => 229,
        "aelig" => 230,
        "ccedil" => 231,
        "egrave" => 232,
        "eacute" => 233,
        "ecirc" => 234,
        "euml" => 235,
        "igrave" => 236,
        "iacute" => 237,
        "icirc" => 238,
        "iuml" => 239,
        "eth" => 240,
        "ntilde" => 241,
        "ograve" => 242,
        "oacute" => 243,
        "ocirc" => 244,
        "otilde" => 245,
        "ouml" => 246,
        "divide" => 247,
        "oslash" => 248,
        "ugrave" => 249,
        "uacute" => 250,
        "ucirc" => 251,
        "uuml" => 252,
        "yacute" => 253,
        "thorn" => 254,
        "yuml" => 255,
        "OElig" => 338,
        "oelig" => 339,
        "Scaron" => 352,
        "scaron" => 353,
        "Yuml" => 376,
        "fnof" => 402,
        "circ" => 710,
        "tilde" => 732,
        "Alpha" => 913,
        "Beta" => 914,
        "Gamma" => 915,
        "Delta" => 916,
        "Epsilon" => 917,
        "Zeta" => 918,
        "Eta" => 919,
        "Theta" => 920,
        "Iota" => 921,
        "Kappa" => 922,
        "Lambda" => 923,
        "Mu" => 924,
        "Nu" => 925,
        "Xi" => 926,
        "Omicron" => 927,
        "Pi" => 928,
        "Rho" => 929,
        "Sigma" => 931,
        "Tau" => 932,
        "Upsilon" => 933,
        "Phi" => 934,
        "Chi" => 935,
        "Psi" => 936,
        "Omega" => 937,
        "alpha" => 945,
        "beta" => 946,
        "gamma" => 947,
        "delta" => 948,
        "epsilon" => 949,
        "zeta" => 950,
        "eta" => 951,
        "theta" => 952,
        "iota" => 953,
        "kappa" => 954,
        "lambda" => 955,
        "mu" => 956,
        "nu" => 957,
        "xi" => 958,
        "omicron" => 959,
        "pi" => 960,
        "rho" => 961,
        "sigmaf" => 962,
        "sigma" => 963,
        "tau" => 964,
        "upsilon" => 965,
        "phi" => 966,
        "chi" => 967,
        "psi" => 968,
        "omega" => 969,
        "thetasym" => 977,
        "upsih" => 978,
        "piv" => 982,
        "ensp" => 8194,
        "emsp" => 8195,
        "thinsp" => 8201,
        "zwnj" => 8204,
        "zwj" => 8205,
        "lrm" => 8206,
        "rlm" => 8207,
        "ndash" => 8211,
        "mdash" => 8212,
        "lsquo" => 8216,
        "rsquo" => 8217,
        "sbquo" => 8218,
        "ldquo" => 8220,
        "rdquo" => 8221,
        "bdquo" => 8222,
        "dagger" => 8224,
        "Dagger" => 8225,
        "bull" => 8226,
        "hellip" => 8230,
        "permil" => 8240,
        "prime" => 8242,
        "Prime" => 8243,
        "lsaquo" => 8249,
        "rsaquo" => 8250,
        "oline" => 8254,
        "frasl" => 8260,
        "euro" => 8364,
        "image" => 8465,
        "weierp" => 8472,
        "real" => 8476,
        "trade" => 8482,
        "alefsym" => 8501,
        "larr" => 8592,
        "uarr" => 8593,
        "rarr" => 8594,
        "darr" => 8595,
        "harr" => 8596,
        "crarr" => 8629,
        "lArr" => 8656,
        "uArr" => 8657,
        "rArr" => 8658,
        "dArr" => 8659,
        "hArr" => 8660,
        "forall" => 8704,
        "part" => 8706,
        "exist" => 8707,
        "empty" => 8709,
        "nabla" => 8711,
        "isin" => 8712,
        "notin" => 8713,
        "ni" => 8715,
        "prod" => 8719,
        "sum" => 8721,
        "minus" => 8722,
        "lowast" => 8727,
        "radic" => 8730,
        "prop" => 8733,
        "infin" => 8734,
        "ang" => 8736,
        "and" => 8743,
        "or" => 8744,
        "cap" => 8745,
        "cup" => 8746,
        "int" => 8747,
        "there4" => 8756,
        "sim" => 8764,
        "cong" => 8773,
        "asymp" => 8776,
        "ne" => 8800,
        "equiv" => 8801,
        "le" => 8804,
        "ge" => 8805,
        "sub" => 8834,
        "sup" => 8835,
        "nsub" => 8836,
        "sube" => 8838,
        "supe" => 8839,
        "oplus" => 8853,
        "otimes" => 8855,
        "perp" => 8869,
        "sdot" => 8901,
        "lceil" => 8968,
        "rceil" => 8969,
        "lfloor" => 8970,
        "rfloor" => 8971,
        "lang" => 9001,
        "rang" => 9002,
        "loz" => 9674,
        "spades" => 9824,
        "clubs" => 9827,
        "hearts" => 9829,
        "diams" => 9830,
        _ => return None,
    };
    char::from_u32(value)
}

fn is_tag_letter_or_digit(ch: char) -> bool {
    if ch.is_ascii() {
        return ch.is_ascii_alphanumeric();
    }
    // ReadTag classifies UTF-16 chars, not runes. Rust's alphanumeric property
    // additionally includes Other_Alphabetic marks, Nl, and No. Exclude those
    // BMP categories to retain precisely the source's L* / Nd classification.
    let cp = ch as u32;
    cp <= 0xffff
        && ch.is_alphanumeric()
        && !NON_LETTER_DIGIT
            .iter()
            .any(|&(start, end)| (start..=end).contains(&cp))
}

// Unicode 16 BMP Alphabetic/Number characters outside the L* and Nd categories.
const NON_LETTER_DIGIT: &[(u32, u32)] = &[
    (0x00b2, 0x00b3),
    (0x00b9, 0x00b9),
    (0x00bc, 0x00be),
    (0x0345, 0x0345),
    (0x05b0, 0x05bd),
    (0x05bf, 0x05bf),
    (0x05c1, 0x05c2),
    (0x05c4, 0x05c5),
    (0x05c7, 0x05c7),
    (0x0610, 0x061a),
    (0x064b, 0x0657),
    (0x0659, 0x065f),
    (0x0670, 0x0670),
    (0x06d6, 0x06dc),
    (0x06e1, 0x06e4),
    (0x06e7, 0x06e8),
    (0x06ed, 0x06ed),
    (0x0711, 0x0711),
    (0x0730, 0x073f),
    (0x07a6, 0x07b0),
    (0x0816, 0x0817),
    (0x081b, 0x0823),
    (0x0825, 0x0827),
    (0x0829, 0x082c),
    (0x0897, 0x0897),
    (0x08d4, 0x08df),
    (0x08e3, 0x08e9),
    (0x08f0, 0x0903),
    (0x093a, 0x093b),
    (0x093e, 0x094c),
    (0x094e, 0x094f),
    (0x0955, 0x0957),
    (0x0962, 0x0963),
    (0x0981, 0x0983),
    (0x09be, 0x09c4),
    (0x09c7, 0x09c8),
    (0x09cb, 0x09cc),
    (0x09d7, 0x09d7),
    (0x09e2, 0x09e3),
    (0x09f4, 0x09f9),
    (0x0a01, 0x0a03),
    (0x0a3e, 0x0a42),
    (0x0a47, 0x0a48),
    (0x0a4b, 0x0a4c),
    (0x0a51, 0x0a51),
    (0x0a70, 0x0a71),
    (0x0a75, 0x0a75),
    (0x0a81, 0x0a83),
    (0x0abe, 0x0ac5),
    (0x0ac7, 0x0ac9),
    (0x0acb, 0x0acc),
    (0x0ae2, 0x0ae3),
    (0x0afa, 0x0afc),
    (0x0b01, 0x0b03),
    (0x0b3e, 0x0b44),
    (0x0b47, 0x0b48),
    (0x0b4b, 0x0b4c),
    (0x0b56, 0x0b57),
    (0x0b62, 0x0b63),
    (0x0b72, 0x0b77),
    (0x0b82, 0x0b82),
    (0x0bbe, 0x0bc2),
    (0x0bc6, 0x0bc8),
    (0x0bca, 0x0bcc),
    (0x0bd7, 0x0bd7),
    (0x0bf0, 0x0bf2),
    (0x0c00, 0x0c04),
    (0x0c3e, 0x0c44),
    (0x0c46, 0x0c48),
    (0x0c4a, 0x0c4c),
    (0x0c55, 0x0c56),
    (0x0c62, 0x0c63),
    (0x0c78, 0x0c7e),
    (0x0c81, 0x0c83),
    (0x0cbe, 0x0cc4),
    (0x0cc6, 0x0cc8),
    (0x0cca, 0x0ccc),
    (0x0cd5, 0x0cd6),
    (0x0ce2, 0x0ce3),
    (0x0cf3, 0x0cf3),
    (0x0d00, 0x0d03),
    (0x0d3e, 0x0d44),
    (0x0d46, 0x0d48),
    (0x0d4a, 0x0d4c),
    (0x0d57, 0x0d5e),
    (0x0d62, 0x0d63),
    (0x0d70, 0x0d78),
    (0x0d81, 0x0d83),
    (0x0dcf, 0x0dd4),
    (0x0dd6, 0x0dd6),
    (0x0dd8, 0x0ddf),
    (0x0df2, 0x0df3),
    (0x0e31, 0x0e31),
    (0x0e34, 0x0e3a),
    (0x0e4d, 0x0e4d),
    (0x0eb1, 0x0eb1),
    (0x0eb4, 0x0eb9),
    (0x0ebb, 0x0ebc),
    (0x0ecd, 0x0ecd),
    (0x0f2a, 0x0f33),
    (0x0f71, 0x0f83),
    (0x0f8d, 0x0f97),
    (0x0f99, 0x0fbc),
    (0x102b, 0x1036),
    (0x1038, 0x1038),
    (0x103b, 0x103e),
    (0x1056, 0x1059),
    (0x105e, 0x1060),
    (0x1062, 0x1064),
    (0x1067, 0x106d),
    (0x1071, 0x1074),
    (0x1082, 0x108d),
    (0x108f, 0x108f),
    (0x109a, 0x109d),
    (0x1369, 0x137c),
    (0x16ee, 0x16f0),
    (0x1712, 0x1713),
    (0x1732, 0x1733),
    (0x1752, 0x1753),
    (0x1772, 0x1773),
    (0x17b6, 0x17c8),
    (0x17f0, 0x17f9),
    (0x1885, 0x1886),
    (0x18a9, 0x18a9),
    (0x1920, 0x192b),
    (0x1930, 0x1938),
    (0x19da, 0x19da),
    (0x1a17, 0x1a1b),
    (0x1a55, 0x1a5e),
    (0x1a61, 0x1a74),
    (0x1abf, 0x1ac0),
    (0x1acc, 0x1ace),
    (0x1b00, 0x1b04),
    (0x1b35, 0x1b43),
    (0x1b80, 0x1b82),
    (0x1ba1, 0x1ba9),
    (0x1bac, 0x1bad),
    (0x1be7, 0x1bf1),
    (0x1c24, 0x1c36),
    (0x1de7, 0x1df4),
    (0x2070, 0x2070),
    (0x2074, 0x2079),
    (0x2080, 0x2089),
    (0x2150, 0x2182),
    (0x2185, 0x2189),
    (0x2460, 0x249b),
    (0x24b6, 0x24ff),
    (0x2776, 0x2793),
    (0x2cfd, 0x2cfd),
    (0x2de0, 0x2dff),
    (0x3007, 0x3007),
    (0x3021, 0x3029),
    (0x3038, 0x303a),
    (0x3192, 0x3195),
    (0x3220, 0x3229),
    (0x3248, 0x324f),
    (0x3251, 0x325f),
    (0x3280, 0x3289),
    (0x32b1, 0x32bf),
    (0xa674, 0xa67b),
    (0xa69e, 0xa69f),
    (0xa6e6, 0xa6ef),
    (0xa802, 0xa802),
    (0xa80b, 0xa80b),
    (0xa823, 0xa827),
    (0xa830, 0xa835),
    (0xa880, 0xa881),
    (0xa8b4, 0xa8c3),
    (0xa8c5, 0xa8c5),
    (0xa8ff, 0xa8ff),
    (0xa926, 0xa92a),
    (0xa947, 0xa952),
    (0xa980, 0xa983),
    (0xa9b4, 0xa9bf),
    (0xa9e5, 0xa9e5),
    (0xaa29, 0xaa36),
    (0xaa43, 0xaa43),
    (0xaa4c, 0xaa4d),
    (0xaa7b, 0xaa7d),
    (0xaab0, 0xaab0),
    (0xaab2, 0xaab4),
    (0xaab7, 0xaab8),
    (0xaabe, 0xaabe),
    (0xaaeb, 0xaaef),
    (0xaaf5, 0xaaf5),
    (0xabe3, 0xabea),
    (0xfb1e, 0xfb1e),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_fetcher_html_example() {
        let html = "<html><style>secret</style><body><h1>Hello &amp; bye</h1>\
                    <script>alert(1)</script><p>Text\u{1}</p></body></html>";
        assert_eq!(extract_html(html), "Hello & bye\n\nText");
    }

    #[test]
    fn original_malformed_html_example() {
        let html = "<h1>Title</h1><!-- comment > still comment --><p>A&nbsp; B</p>\
                    <script>bad</script><div title=\">\">C</div>";
        assert_eq!(extract_html(html), "Title\n\nA B\n\nC");
    }

    #[test]
    fn control_and_replacement_runes_are_removed_but_format_runes_survive() {
        assert_eq!(
            strip_controls("a\0\u{1}\u{b}\u{c}\u{1f}\u{7f}\u{85}\u{9f}\u{fffd}\n\r\t\u{200d}🦜z"),
            "a\n\r\t\u{200d}🦜z"
        );
        let decoded = String::from_utf8_lossy(b"a\xff\xc0\x80b\xe2\x82");
        assert_eq!(strip_controls(&decoded), "ab");
    }

    #[test]
    fn hidden_tags_use_one_matching_slot_not_a_nested_stack() {
        assert_eq!(
            extract_html("a<SCRIPT><p>bad</p><style>worse</style>still</SCRIPT>b"),
            "ab"
        );
        assert_eq!(
            extract_html("a<template>x<template>y</template>b</template>c"),
            "abc"
        );
        for tag in ["script", "style", "noscript", "template"] {
            assert_eq!(extract_html(&format!("a<{tag}/>hidden")), "a");
            assert_eq!(extract_html(&format!("a<{tag}>hidden<broken")), "a");
        }
    }

    #[test]
    fn comments_and_unclosed_tags_match_scanner_behavior() {
        assert_eq!(extract_html("a<!-- > ' <p> -->b"), "ab");
        assert_eq!(extract_html("a<!-- never closed <p>b"), "a");
        assert_eq!(extract_html("a<broken &amp; tail"), "a<broken & tail");
        assert_eq!(
            extract_html("a<div title='unterminated>b"),
            "a<div title='unterminated>b"
        );
        assert_eq!(
            extract_html("a<div title='>' other=\">\">b</div>c"),
            "a\nb\nc"
        );
        assert_eq!(extract_html("a<!thing>b<?thing>c<>d</>e"), "abcde");
    }

    #[test]
    fn tag_names_follow_utf16_letter_and_decimal_digit_categories() {
        assert_eq!(read_tag(" / P class='x' "), ("p".into(), true));
        assert_eq!(read_tag("p²"), ("p".into(), false));
        assert_eq!(read_tag("p\u{345}"), ("p".into(), false));
        assert_eq!(read_tag("pⅣ"), ("p".into(), false));
        assert_eq!(read_tag("p𐐀"), ("p".into(), false));
        assert_eq!(read_tag("pÉ١"), ("pé١".into(), false));
        assert_eq!(extract_html("a<p²>b<pÉ>c<p𐐀>d"), "a\nbc\nd");
        assert_eq!(extract_html("a<blocKquote>b"), "a\nb");
    }

    #[test]
    fn whitespace_normalization_keeps_at_most_one_blank_line() {
        assert_eq!(
            extract_html("\n\t a\t b \r\nc \n\n\n d\n\t"),
            "a b\n\nc\n\nd"
        );
        assert_eq!(
            extract_html("a\u{a0}\u{1680}\u{2003}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}b"),
            "a b"
        );
        assert_eq!(extract_html("a\u{85}b\u{b}c\u{c}d"), "abcd");
        assert_eq!(extract_html("\r\n  \t"), "");
    }

    #[test]
    fn html4_names_are_case_sensitive_complete_and_decoded_once() {
        assert_eq!(extract_html("&quot;&amp;&apos;&lt;&gt;"), "\"&'<>");
        assert_eq!(
            extract_html("&Agrave;&agrave;&OElig;&thetasym;&euro;&lArr;&spades;&lang;"),
            "ÀàŒϑ€⇐♠〈"
        );
        assert_eq!(
            extract_html("&AMP; &Amp; &NotEqualTilde; &amp &bogus;"),
            "&AMP; &Amp; &NotEqualTilde; &amp &bogus;"
        );
        assert_eq!(
            extract_html("&amp;lt;b&amp;gt; &lt;b&gt;text&lt;/b&gt;"),
            "&lt;b&gt; <b>text</b>"
        );
        assert_eq!(
            extract_html("&foo&amp;|&#xno&amp;|&am&lt;|&&amp;"),
            "&foo&|&#xno&|&am<|&&"
        );
    }

    #[test]
    fn numeric_entities_preserve_invalid_scalars_and_remove_decoded_controls() {
        assert_eq!(
            extract_html("&#65;&#x42;&#X43;&#00068;&#x1F99C;&#1114111;"),
            "ABCD🦜\u{10ffff}"
        );
        assert_eq!(
            extract_html("a&#0;&#1;&#11;&#12;&#127;&#128;&#x85;&#65533;b"),
            "ab"
        );
        assert_eq!(extract_html("a&#9;b&#10;c&#13;d"), "a b\nc\nd");
        let invalid = "&#xD800;|&#55296;|&#xDFFF;|&#x110000;|&#4294967296;|&#-1;|&#;|&#x;|&#xGG;";
        assert_eq!(extract_html(invalid), invalid);
        assert_eq!(
            extract_html("&# +65;|&#65 ;|&#-0;|&#x+41;|&#x 41;"),
            "A|A||&#x+41;|&#x 41;"
        );
        assert_eq!(html_decode("&#65\0;|&#x41\0;|&#65\0 ;"), "A|A|&#65\0 ;");
    }

    #[test]
    fn every_source_block_tag_is_a_line_boundary_on_open_and_close() {
        for tag in [
            "address",
            "article",
            "aside",
            "blockquote",
            "br",
            "div",
            "dl",
            "fieldset",
            "figcaption",
            "figure",
            "footer",
            "form",
            "h1",
            "h2",
            "h3",
            "h4",
            "h5",
            "h6",
            "header",
            "hr",
            "li",
            "main",
            "nav",
            "ol",
            "p",
            "pre",
            "section",
            "table",
            "tr",
            "ul",
        ] {
            assert_eq!(
                extract_html(&format!("a<{tag}>b</{tag}>c")),
                "a\nb\nc",
                "{tag}"
            );
        }
        assert_eq!(extract_html("a<span>b</span><td>c</td>"), "abc");
    }
}
