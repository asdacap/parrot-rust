/// Inline text and image paths retain their exact order and whitespace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PromptAttachmentIntent {
    Text(String),
    Path(String),
}

/// Parse references only at a whitespace boundary. Braces quote spaces, not an
/// escaping language; malformed references stay text and boundary `@@` is `@`.
pub fn parse(prompt: &str) -> Vec<PromptAttachmentIntent> {
    let mut intents = Vec::new();
    let mut text = String::new();
    let mut index = 0;
    while index < prompt.len() {
        let character = prompt[index..].chars().next().expect("character boundary");
        let boundary = prompt[..index]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace);
        if character == '@' && boundary {
            let rest = &prompt[index + 1..];
            if rest.starts_with('@') {
                text.push('@');
                index += 2;
                continue;
            }
            let path = if let Some(braced) = rest.strip_prefix('{') {
                braced.find('}').and_then(|end| {
                    let path = &braced[..end];
                    (!path.trim().is_empty()).then_some((path, index + end + 3))
                })
            } else {
                let length = rest.find(char::is_whitespace).unwrap_or(rest.len());
                (length > 0).then_some((&rest[..length], index + length + 1))
            };
            if let Some((path, next)) = path {
                if !text.is_empty() {
                    intents.push(PromptAttachmentIntent::Text(std::mem::take(&mut text)));
                }
                intents.push(PromptAttachmentIntent::Path(path.into()));
                index = next;
                continue;
            }
        }
        text.push(character);
        index += character.len_utf8();
    }
    if !text.is_empty() {
        intents.push(PromptAttachmentIntent::Text(text));
    }
    intents
}
