use super::Model;

const ALLOWED: &[&str] = &["gpt-5.5", "gpt-5.3-codex-spark", "gpt-5.4", "gpt-5.4-mini"];
const DISALLOWED: &[&str] = &["gpt-5.5-pro", "gpt-5.6"];

#[must_use]
pub fn filter_chatgpt_external(models: &[Model], chatgpt_provider_id: &str) -> Vec<Model> {
    models
        .iter()
        .filter(|model| includes(&model.id))
        .cloned()
        .map(|mut model| {
            model.provider_id = chatgpt_provider_id.to_owned();
            model
        })
        .collect()
}

fn includes(model_id: &str) -> bool {
    if ALLOWED.contains(&model_id) {
        return true;
    }
    if DISALLOWED.contains(&model_id) || !model_id.starts_with("gpt-") {
        return false;
    }
    let version = &model_id[4..];
    let major_length = digit_prefix_len(version);
    if major_length == 0 {
        return false;
    }
    let major = &version[..major_length];
    if greater_than(major, 5) {
        return true;
    }
    if !equal_to(major, 5) {
        return false;
    }
    let suffix = &version[major_length..];
    if suffix.len() < 2 || !suffix.starts_with('.') || !suffix.as_bytes()[1].is_ascii_digit() {
        return false;
    }
    let minor = &suffix[1..1 + digit_prefix_len(&suffix[1..])];
    !minor.is_empty() && greater_than(minor, 4)
}

fn digit_prefix_len(value: &str) -> usize {
    value.bytes().take_while(u8::is_ascii_digit).count()
}

fn trim_zeroes(value: &str) -> &str {
    let trimmed = value.trim_start_matches('0');
    if trimmed.is_empty() { "0" } else { trimmed }
}

fn equal_to(digits: &str, value: u8) -> bool {
    let digits = trim_zeroes(digits).as_bytes();
    digits.len() == 1 && digits[0] - b'0' == value
}

fn greater_than(digits: &str, value: u8) -> bool {
    let digits = trim_zeroes(digits).as_bytes();
    digits.len() > 1 || digits[0] - b'0' > value
}
