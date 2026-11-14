use std::time::{Duration, SystemTime};

pub(crate) fn parse<'a>(
    mut values: impl Iterator<Item = &'a str>,
    received_at: SystemTime,
) -> Option<Duration> {
    let value = values.next()?.trim_matches([' ', '\t']);
    if values.next().is_some() || value.is_empty() {
        return None;
    }
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse().ok().map(Duration::from_secs);
    }
    httpdate::parse_http_date(value)
        .ok()
        .map(|deadline| deadline.duration_since(received_at).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_delay_seconds_and_http_dates() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(784111777);
        for (value, expected) in [
            ("15", 15),
            (" \t10\t ", 10),
            ("0", 0),
            ("Sun, 06 Nov 1994 08:49:52 GMT", 15),
            ("Sunday, 06-Nov-94 08:49:52 GMT", 15),
            ("Sun Nov  6 08:49:52 1994", 15),
            ("Sun, 06 Nov 1994 08:49:37 GMT", 0),
        ] {
            assert_eq!(
                parse([value].into_iter(), now),
                Some(Duration::from_secs(expected))
            );
        }
    }

    #[test]
    fn invalid_or_ambiguous_values_have_no_cooldown() {
        let now = SystemTime::UNIX_EPOCH;
        for value in [
            "",
            "-1",
            "+1",
            "1.5",
            "later",
            "18446744073709551616",
            "10, 15",
            "15\n",
        ] {
            assert_eq!(parse([value].into_iter(), now), None);
        }
        assert_eq!(parse(["10", "15"].into_iter(), now), None);
        assert_eq!(parse(std::iter::empty(), now), None);
        assert_eq!(
            parse(["18446744073709551615"].into_iter(), now),
            Some(Duration::from_secs(u64::MAX))
        );
    }
}
