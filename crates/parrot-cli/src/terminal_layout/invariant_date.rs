// The .NET Foundation licenses the source algorithm under the MIT license.
// Copyright (c) .NET Foundation and Contributors.
// Derived from dotnet/runtime v10.0.10 invariant/Hebrew-token parsing and Unix timezone rules.
// Copyright and license notices are in THIRD_PARTY_NOTICES.md.
// for source URLs, hashes, attribution and the bounded projection.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, Utc};

// HebrewNumber.ParseByChar: preserve all source token/state ordinals.
const HEBREW_STATES: [[i8; 10]; 17] = [
    [1, 10, 10, 9, 8, 8, 8, 14, -1, -1],              // Start
    [2, 3, 3, 4, -1, -1, -1, 15, 100, 11],            // S400
    [-1, -1, 13, 4, -1, -1, -1, 15, -1, 12],          // S400_400
    [-1, -1, -1, 6, -1, -1, -1, 15, -1, 5],           // S400_X00
    [-1, -1, -1, -1, -1, -1, -1, -1, -1, 7],          // S400_X0
    [-1, -1, -1, 100, 100, 100, 100, 100, -1, -1],    // X00_DQ
    [-1, -1, -1, -1, -1, -1, -1, -1, -1, 7],          // S400_X00_X0
    [-1, -1, -1, -1, 100, 100, 100, 100, -1, -1],     // X0_DQ
    [-1, -1, -1, -1, -1, -1, -1, -1, 100, -1],        // X
    [-1, -1, -1, -1, -1, -1, -1, -1, 100, 7],         // X0
    [-1, -1, -1, 4, -1, -1, -1, 15, 100, 5],          // X00
    [100, 100, 100, 100, 100, 100, 100, 100, -1, -1], // S400_DQ
    [-1, -1, 100, 100, 100, 100, 100, 100, -1, -1],   // S400_400_DQ
    [-1, -1, -1, 6, -1, -1, -1, 15, -1, 5],           // S400_400_100
    [-1, -1, -1, -1, -1, -1, -1, -1, 100, 16],        // S9
    [-1, -1, -1, -1, -1, -1, -1, -1, -1, 16],         // X00_S9
    [-1, -1, -1, -1, -1, 100, 100, -1, -1, -1],       // S9_DQ
];
const HEBREW_VALUES: [(i8, i32); 27] = [
    (4, 1),
    (4, 2),
    (4, 3),
    (4, 4),
    (4, 5),
    (5, 6),
    (5, 7),
    (4, 8),
    (7, 9),
    (3, 10),
    (-1, -1),
    (3, 20),
    (3, 30),
    (-1, -1),
    (3, 40),
    (-1, -1),
    (3, 50),
    (3, 60),
    (3, 70),
    (-1, -1),
    (3, 80),
    (-1, -1),
    (3, 90),
    (2, 100),
    (1, 200),
    (1, 300),
    (0, 400),
];

const TICKS_SECOND: i64 = 10_000_000;
const TICKS_DAY: i64 = 86400 * TICKS_SECOND;
const MAX_TICKS: i64 = 3_155_378_975_999_999_999;
const MAX_OFFSET: i64 = 14 * 3600 * TICKS_SECOND;

// DTT and DS retain the upstream ordinals. Keeping the full table makes the
// lexical/terminal distinction explicit, including the globally registered CJK
// suffix states (not just the English invariant format patterns).
const STATES: [[u8; 18]; 20] = [
    [
        0, 20, 34, 1, 3, 18, 20, 6, 6, 16, 17, 0, 11, 11, 20, 0, 0, 20,
    ],
    [
        20, 21, 35, 2, 5, 20, 24, 8, 9, 10, 20, 1, 12, 13, 32, 1, 1, 20,
    ],
    [
        21, 22, 36, 22, 20, 18, 25, 25, 20, 20, 17, 2, 38, 20, 38, 2, 2, 20,
    ],
    [
        20, 21, 20, 4, 5, 20, 24, 7, 9, 20, 20, 3, 12, 13, 32, 20, 3, 20,
    ],
    [
        21, 22, 34, 22, 20, 18, 25, 25, 20, 26, 17, 4, 38, 20, 38, 20, 4, 20,
    ],
    [
        20, 22, 22, 22, 20, 20, 25, 25, 20, 26, 20, 5, 38, 20, 38, 20, 5, 20,
    ],
    [
        20, 23, 20, 7, 9, 20, 20, 20, 20, 20, 20, 6, 14, 15, 33, 20, 6, 20,
    ],
    [
        23, 25, 25, 25, 20, 18, 20, 20, 20, 26, 17, 7, 31, 20, 31, 20, 7, 20,
    ],
    [
        24, 25, 25, 25, 20, 18, 20, 20, 20, 26, 17, 8, 31, 20, 31, 20, 8, 20,
    ],
    [
        20, 25, 20, 25, 20, 20, 20, 20, 20, 20, 20, 9, 31, 20, 31, 20, 9, 20,
    ],
    [
        28, 29, 29, 29, 20, 18, 20, 20, 20, 10, 17, 10, 20, 20, 20, 20, 10, 20,
    ],
    [
        20, 32, 20, 12, 13, 20, 33, 14, 15, 14, 20, 11, 20, 20, 20, 20, 11, 20,
    ],
    [
        32, 30, 30, 30, 20, 20, 31, 31, 20, 20, 20, 12, 20, 20, 20, 20, 12, 20,
    ],
    [
        20, 30, 30, 30, 20, 20, 31, 31, 20, 20, 20, 12, 20, 20, 20, 20, 12, 20,
    ],
    [
        33, 31, 31, 31, 20, 20, 20, 20, 20, 20, 20, 14, 20, 20, 20, 20, 14, 20,
    ],
    [
        20, 31, 31, 31, 20, 20, 20, 20, 20, 20, 20, 14, 20, 20, 20, 20, 14, 20,
    ],
    [
        26, 27, 34, 18, 20, 18, 20, 20, 20, 16, 17, 16, 20, 20, 20, 20, 16, 20,
    ],
    [
        37, 37, 37, 18, 3, 20, 20, 20, 20, 16, 17, 17, 20, 20, 20, 17, 17, 20,
    ],
    [
        20, 35, 35, 35, 20, 19, 24, 8, 20, 20, 17, 20, 20, 20, 20, 18, 18, 35,
    ],
    [
        20, 36, 36, 36, 20, 20, 20, 20, 20, 20, 17, 19, 20, 20, 20, 19, 19, 36,
    ],
];
const END: usize = 0;
const NUM_END: usize = 1;
const NUM_AMPM: usize = 2;
const NUM_SPACE: usize = 3;
const NUM_DATE: usize = 4;
const NUM_TIME: usize = 5;
const MONTH_END: usize = 6;
const MONTH_SPACE: usize = 7;
const MONTH_DATE: usize = 8;
const DATE_SUFFIX: usize = 9;
const TIME_SUFFIX: usize = 10;
const WEEKDAY: usize = 11;
const YEAR_SPACE: usize = 12;
const YEAR_DATE: usize = 13;
const YEAR_END: usize = 14;
const ZONE: usize = 15;
const ERA: usize = 16;
const UNKNOWN: usize = 18;
const LOCAL_MARK: usize = 19;

#[derive(Default)]
struct Raw {
    numbers: Vec<i32>,
    year: Option<i32>,
    month: Option<i32>,
    weekday: Option<u32>,
    pm: Option<bool>,
    fraction: f64,
}

#[derive(Default)]
struct Parsed {
    fields: [Option<i32>; 6],
    suffix_fields: u8,
    have_date: bool,
    have_time: bool,
    default_year: bool,
    offset: Option<i32>, // seconds, including offsets >14h until postprocessing
    clock: Option<(chrono::DateTime<Utc>, NaiveDate)>,
}

#[derive(Clone, Copy)]
enum Separator {
    End,
    Space,
    Date,
    Offset,
    Time,
    Meridiem(bool),
    Suffix(usize),
    Local,
}

struct Lexer<'a> {
    input: &'a str,
    pos: usize,
    pending_fraction: f64,
}

impl<'a> Lexer<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            input,
            pos: 0,
            pending_fraction: 0.0,
        }
    }

    fn rest(&self) -> &'a str {
        &self.input[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn skip_space(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.pos += self.peek().unwrap().len_utf8();
        }
    }

    fn consume(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += c.len_utf8();
            true
        } else {
            false
        }
    }

    fn word(&mut self, word: &str) -> bool {
        let Some(prefix) = self.rest().get(..word.len()) else {
            return false;
        };
        if prefix.eq_ignore_ascii_case(word)
            && !self.rest()[word.len()..]
                .chars()
                .next()
                .is_some_and(is_letter)
        {
            self.pos += word.len();
            true
        } else {
            false
        }
    }

    fn number(&mut self) -> Option<(i32, usize)> {
        let length = self.rest().bytes().take_while(u8::is_ascii_digit).count();
        if length == 0 || length > 8 {
            return None;
        }
        let n = self.rest()[..length].parse().ok()?;
        self.pos += length;
        Some((n, length))
    }

    fn hebrew_number(&mut self) -> Option<i32> {
        let mut state = 0;
        let mut number = 0;
        for (position, c) in self.rest().char_indices() {
            let token = match c {
                '\'' => 8,
                '"' => 9,
                _ => {
                    let index = (c as u32).checked_sub(0x05d0)? as usize;
                    let &(token, value) = HEBREW_VALUES.get(index)?;
                    if token < 0 {
                        return None;
                    }
                    number += value;
                    token as usize
                }
            };
            state = HEBREW_STATES[state as usize][token];
            if state == -1 {
                return None;
            }
            if state == 100 {
                self.pos += position + c.len_utf8();
                return Some(number);
            }
        }
        None
    }

    fn separator(&mut self) -> Separator {
        // GetSeparatorToken alone uses SkipWhiteSpaceAndRtlMarkCurrent.
        // Regular tokens and ISO/offset parsing do not globally ignore marks.
        while self
            .peek()
            .is_some_and(|c| c.is_whitespace() || c == '\u{200f}')
        {
            self.pos += self.peek().unwrap().len_utf8();
        }
        if self.peek().is_none() {
            return Separator::End;
        }
        if self.word("AM") {
            return Separator::Meridiem(false);
        }
        if self.word("PM") {
            return Separator::Meridiem(true);
        }
        let c = self.peek().unwrap();
        let separator = match c {
            '/' => Separator::Date,
            '-' => Separator::Offset,
            ':' => Separator::Time,
            'T' | 't' if !self.rest()[1..].chars().next().is_some_and(is_letter) => {
                Separator::Local
            }
            '年' | '년' => Separator::Suffix(0),
            '月' | '월' => Separator::Suffix(1),
            '日' | '일' => Separator::Suffix(2),
            '時' | '时' => Separator::Suffix(3),
            '分' => Separator::Suffix(4),
            '秒' => Separator::Suffix(5),
            _ => return Separator::Space,
        };
        // CJK suffixes are alphabetic too: invariant tokenization does not
        // allow one letter-token immediately followed by another letter.
        if matches!(separator, Separator::Suffix(_))
            && self.rest()[c.len_utf8()..]
                .chars()
                .next()
                .is_some_and(is_letter)
        {
            return Separator::Space;
        }
        self.pos += c.len_utf8();
        separator
    }

    fn fraction(&mut self) -> bool {
        self.pos += 1; // dot or comma
        let mut base = 0.1;
        let mut fraction = 0.0;
        let start = self.pos;
        while let Some(digit) = self
            .rest()
            .as_bytes()
            .first()
            .filter(|b| b.is_ascii_digit())
        {
            fraction += f64::from(*digit - b'0') * base;
            base *= 0.1;
            self.pos += 1;
        }
        self.pending_fraction = fraction;
        self.pos > start
    }

    fn offset(&mut self) -> Option<i32> {
        let negative = match self.peek()? {
            '-' => true,
            '+' => false,
            _ => return None,
        };
        self.pos += 1;
        let (n, width) = self.number()?;
        let (hours, minutes) = match width {
            1 | 2 if self.consume(':') => {
                let (minutes, width) = self.number()?;
                if width > 2 {
                    return None;
                }
                (n, minutes)
            }
            1 | 2 => (n, 0),
            3 | 4 => (n / 100, n % 100),
            _ => return None,
        };
        if minutes >= 60 {
            return None;
        }
        Some((hours * 3600 + minutes * 60) * if negative { -1 } else { 1 })
    }

    fn handle_offset(&mut self, parsed: &mut Parsed) -> Option<()> {
        let start = self.pos;
        self.skip_space();
        if matches!(self.peek(), Some('+' | '-')) && self.rest().len() > 1 {
            if parsed.offset.is_some() {
                return None;
            }
            parsed.offset = Some(self.offset()?);
        } else {
            self.pos = start;
        }
        Some(())
    }
}

// .NET char.IsLetter excludes combining marks, letter-numbers and surrogates.
fn is_letter(c: char) -> bool {
    use unicode_general_category::{GeneralCategory, get_general_category};
    matches!(
        get_general_category(c),
        GeneralCategory::UppercaseLetter
            | GeneralCategory::LowercaseLetter
            | GeneralCategory::TitlecaseLetter
            | GeneralCategory::ModifierLetter
            | GeneralCategory::OtherLetter
    )
}

fn expand_year(n: i32) -> i32 {
    if n < 100 {
        n + if n <= 49 { 2000 } else { 1900 }
    } else {
        n
    }
}

fn date(year: i32, month: i32, day: i32) -> Option<NaiveDate> {
    if !(1..=9999).contains(&year) {
        return None;
    }
    NaiveDate::from_ymd_opt(year, month.try_into().ok()?, day.try_into().ok()?)
}

impl Parsed {
    fn set_date(&mut self, year: i32, month: i32, day: i32) -> Option<()> {
        date(year, month, day)?;
        self.fields[..3].copy_from_slice(&[Some(year), Some(month), Some(day)]);
        Some(())
    }

    fn today(&self) -> NaiveDate {
        if let Some(offset) = self.offset {
            let utc = self.clock.map_or_else(Utc::now, |clock| clock.0);
            (utc + Duration::seconds(i64::from(offset))).date_naive()
        } else {
            self.clock.map_or_else(
                || {
                    let utc = Utc::now();
                    let offset = local_zone().at_utc(utc.timestamp()).unwrap_or(0);
                    (utc + Duration::seconds(i64::from(offset))).date_naive()
                },
                |clock| clock.1,
            )
        }
    }

    fn terminal(&mut self, state: u8, raw: &mut Raw) -> Option<()> {
        let n = |index| raw.numbers.get(index).copied();
        match state {
            21..=25 | 30..=33 | 38 => {
                if self.have_date {
                    return None;
                }
                match state {
                    21 | 23 => {
                        let year = self.today().year();
                        self.default_year = true;
                        self.set_date(
                            year,
                            if state == 21 { n(0)? } else { raw.month? },
                            if state == 21 { n(1)? } else { n(0)? },
                        )?;
                    }
                    // With invariant MD month-day and YM year-month patterns,
                    // GetDayOfNM interprets the number as the year, not day.
                    24 => self.set_date(expand_year(n(0)?), raw.month?, 1)?,
                    22 => self.set_date(expand_year(n(2)?), n(0)?, n(1)?)?,
                    25 => {
                        if self
                            .set_date(expand_year(n(1)?), raw.month?, n(0)?)
                            .is_none()
                        {
                            self.set_date(expand_year(n(0)?), raw.month?, n(1)?)?;
                        }
                    }
                    30 | 38 => self.set_date(raw.year?, n(0)?, n(1)?)?,
                    31 => self.set_date(raw.year?, raw.month?, n(0)?)?,
                    32 => self.set_date(raw.year?, n(0)?, 1)?,
                    33 => self.set_date(raw.year?, raw.month?, 1)?,
                    _ => return None,
                }
                // These two source actions intentionally do not set HaveDate.
                if !matches!(state, 23 | 24) {
                    self.have_date = true;
                }
            }
            34..=36 => {
                if self.have_time || state == 34 && raw.pm.is_none() {
                    return None;
                }
                self.fields[3] = Some(n(0)?);
                if state >= 35 {
                    self.fields[4] = Some(n(1)?);
                }
                if state >= 36 {
                    self.fields[5] = Some(n(2)?);
                }
                self.have_time = true;
            }
            26 | 37 => {}
            27 => {
                if raw.numbers.len() != 1 || self.fields[2].is_some() {
                    return None;
                }
                self.fields[2] = Some(n(0)?);
            }
            28 => {
                self.fields[1]?;
                if self.fields[0].is_some() {
                    return None;
                }
                self.fields[0] = Some(expand_year(n(0)?));
                self.fields[2] = Some(1);
            }
            29 => {
                if self.suffix_fields & 7 == 1 {
                    self.set_date(expand_year(raw.year?), n(0)?, n(1)?)?;
                } else if self.suffix_fields & 7 == 2 {
                    self.set_date(expand_year(n(1)?), self.fields[1]?, n(0)?)?;
                } else {
                    return None;
                }
            }
            _ => return None,
        }
        raw.numbers.clear();
        Some(())
    }

    fn finish(&mut self, raw: &Raw, iso: bool) -> Option<Result<bool, ()>> {
        if !iso {
            if let Some(pm) = raw.pm {
                let hour = self.fields[3].unwrap_or(0);
                self.fields[3] = Some(if pm {
                    if !(0..=23).contains(&hour) {
                        return None;
                    }
                    if hour < 12 { hour + 12 } else { hour }
                } else {
                    if !(0..=12).contains(&hour) {
                        return None;
                    }
                    if hour == 12 { 0 } else { hour }
                });
            }
            if self.offset.is_some()
                && (self.fields[1].is_some() || self.fields[2].is_some())
                && (self.fields[0].is_none() || self.default_year)
            {
                return None;
            }
            if self.fields[..3].iter().all(Option::is_none) {
                let today = self.today();
                self.fields[..3].copy_from_slice(&[
                    Some(today.year()),
                    Some(today.month() as i32),
                    Some(today.day() as i32),
                ]);
            } else {
                if self.fields[0].is_none() {
                    self.fields[0] = Some(self.today().year());
                }
                self.fields[1].get_or_insert(1);
                self.fields[2].get_or_insert(1);
            }
        }
        let day = date(self.fields[0]?, self.fields[1]?, self.fields[2]?)?;
        let time = day.and_hms_opt(
            self.fields[3].unwrap_or(0).try_into().ok()?,
            self.fields[4].unwrap_or(0).try_into().ok()?,
            self.fields[5].unwrap_or(0).try_into().ok()?,
        )?;
        // Mirror ParseFraction's iterative binary64 arithmetic and Math.Round
        // (ties to even), rather than treating decimal text as exact ticks.
        let fraction = (raw.fraction * TICKS_SECOND as f64).round_ties_even() as i64;
        let ticks = i64::from(day.num_days_from_ce() - 1) * TICKS_DAY
            + (time - day.and_hms_opt(0, 0, 0)?).num_seconds() * TICKS_SECOND
            + fraction;
        if !(0..=MAX_TICKS).contains(&ticks) {
            return None;
        }
        if !iso
            && raw
                .weekday
                .is_some_and(|weekday| ((ticks / TICKS_DAY + 1) % 7) as u32 != weekday)
        {
            return None;
        }
        if let Some(offset) = self.offset {
            let offset = i64::from(offset) * TICKS_SECOND;
            return Some(Ok(
                offset.abs() <= MAX_OFFSET && (0..=MAX_TICKS).contains(&(ticks - offset))
            ));
        }
        let time = NaiveDate::from_ymd_opt(1, 1, 1)?.and_hms_opt(0, 0, 0)?
            + Duration::seconds(ticks / TICKS_SECOND)
            + Duration::nanoseconds(ticks % TICKS_SECOND * 100);
        Some(local_boundary_valid(time, ticks))
    }
}

// Local offset/default validation needs the timezone. The file
// selection and offset normalization below project TimeZoneInfo.Unix; tz-rs
// supplies the bounded TZif/POSIX arithmetic primitive, never date recognition.
#[derive(Clone, Copy)]
struct FutureTransitions {
    standard: i64,
    daylight: i64,
    default_daylight: bool,
    start: tz::timezone::RuleDay,
    start_ticks: i64,
    end: tz::timezone::RuleDay,
    end_ticks: i64,
}

#[derive(Clone, Copy)]
enum FutureRule {
    Fixed(i64),
    Alternate(FutureTransitions),
}

type DecodedZone = (tz::TimeZone, Result<Option<FutureRule>, ()>);

struct SourceZone {
    history: tz::TimeZone,
    future: Option<FutureRule>,
    base: i32,
}

impl SourceZone {
    fn future_active_at(&self, time: NaiveDateTime) -> bool {
        let reference = self.history.as_ref();
        let transitions = reference.transitions();
        let Some(last) = transitions.last() else {
            return false;
        };
        let previous_type = if transitions.len() == 1 {
            0
        } else {
            transitions[transitions.len() - 2].local_time_type_index()
        };
        let previous = reference.local_time_types()[previous_type];
        time.and_utc().timestamp() - i64::from(previous.ut_offset()) >= last.unix_leap_time()
    }

    fn at_utc(&self, timestamp: i64) -> Option<i32> {
        let after_history = self
            .history
            .as_ref()
            .transitions()
            .last()
            .is_some_and(|last| timestamp >= last.unix_leap_time());
        if after_history {
            match self.future? {
                FutureRule::Fixed(offset) => return Some(offset as i32),
                FutureRule::Alternate(rule) => {
                    // This path supplies only the current local date default.
                    // Project GetIsDaylightSavingsFromUtc / CheckIsDst, without
                    // a second POSIX parser's name, range or consistency rules.
                    let utc = chrono::DateTime::from_timestamp(timestamp, 0)?.naive_utc();
                    let year = (utc + Duration::seconds(i64::from(self.base))).year();
                    let mut start = rule_time(year, &rule.start, rule.start_ticks)?;
                    let mut end = rule_time(year, &rule.end, rule.end_ticks)?;
                    let mut ignore_year = false;
                    if rule_marker(&rule.start, rule.start_ticks) {
                        let previous =
                            NaiveDate::from_ymd_opt(year - 1, 12, 31)?.and_hms_opt(0, 0, 0)?;
                        let previous_start = rule_time(year - 1, &rule.start, rule.start_ticks)?;
                        let previous_end = rule_time(year - 1, &rule.end, rule.end_ticks)?;
                        if year > 1
                            && self.future_active_at(previous)
                            && (rule_marker(&rule.end, rule.end_ticks)
                                || previous_start > previous_end)
                        {
                            start = previous_start;
                            ignore_year = true;
                        } else {
                            start = NaiveDate::from_ymd_opt(year, 1, 1)?.and_hms_opt(0, 0, 0)?;
                        }
                    }
                    start -= Duration::seconds(rule.standard);
                    if rule_marker(&rule.end, rule.end_ticks) && year < 9999 {
                        if rule_marker(&rule.start, rule.start_ticks) {
                            end =
                                NaiveDate::from_ymd_opt(year + 1, 12, 31)?.and_hms_opt(0, 0, 0)?;
                            ignore_year = true;
                        } else {
                            end = NaiveDate::from_ymd_opt(year + 1, 1, 1)?.and_hms_opt(0, 0, 0)?
                                - Duration::nanoseconds(100);
                        }
                    }
                    end -= Duration::seconds(rule.daylight);
                    // CheckIsDst skips alignment when adjacent-rule continuity
                    // extended the interval beyond this calendar year.
                    let time = if ignore_year {
                        utc
                    } else {
                        end = with_year(end, start.year())?;
                        with_year(utc, start.year())?
                    };
                    let dst = if start > end {
                        time < end || time >= start
                    } else {
                        time >= start && time < end
                    };
                    return Some(if dst { rule.daylight } else { rule.standard } as i32);
                }
            }
        }
        self.history
            .find_local_time_type(timestamp)
            .ok()
            .map(|kind| kind.ut_offset())
    }
}

fn local_zone() -> &'static SourceZone {
    static ZONE: std::sync::OnceLock<SourceZone> = std::sync::OnceLock::new();
    ZONE.get_or_init(|| {
        source_zone_bytes()
            .and_then(|selected| {
                // Source recognizes the selected ID before decoding even a
                // non-UTC or malformed TZif payload for a UTC alias.
                if is_utc_alias(&selected.id) {
                    Some((tz::TimeZone::utc(), Ok(None)))
                } else {
                    decode_zone(&selected.bytes)
                }
            })
            .and_then(|(zone, future)| normalize_zone(zone, future, Utc::now().timestamp()))
            .unwrap_or_else(|| SourceZone {
                history: tz::TimeZone::utc(),
                future: None,
                base: 0,
            })
    })
}

fn tzif_block_end(bytes: &[u8], header: usize, width: usize) -> Option<usize> {
    let header_bytes = bytes.get(header..header.checked_add(44)?)?;
    if header_bytes.get(..4)? != b"TZif" {
        return None;
    }
    let count = |at: usize| -> Option<usize> {
        usize::try_from(u32::from_be_bytes(
            header_bytes.get(at..at + 4)?.try_into().ok()?,
        ))
        .ok()
    };
    let size = count(32)?
        .checked_mul(width + 1)?
        .checked_add(count(36)?.checked_mul(6)?)?
        .checked_add(count(40)?)?
        .checked_add(count(28)?.checked_mul(width + 4)?)?
        .checked_add(count(24)?)?
        .checked_add(count(20)?)?;
    let end = header.checked_add(44)?.checked_add(size)?;
    bytes.get(..end)?;
    Some(end)
}

fn decode_zone(bytes: &[u8]) -> Option<DecodedZone> {
    if !matches!(bytes.get(4), Some(b'2' | b'3')) {
        return Some((tz::TimeZone::from_tz_data(bytes).ok()?, Ok(None)));
    }
    // Only split the standard TZif blocks; tz-rs still decodes/validates all
    // historical transitions. Source parses v2/v3 footers identically and does
    // not require the footer to agree with the last historical type.
    let second = tzif_block_end(bytes, 0, 4)?;
    let footer = tzif_block_end(bytes, second, 8)?;
    let mut history = bytes[..footer].to_vec();
    history.extend_from_slice(b"\n\n");
    let history = tz::TimeZone::from_tz_data(&history).ok()?;
    let tail = bytes.get(footer..)?;
    let future = if tail.first() == Some(&b'\n') && tail.last() == Some(&b'\n') && tail.len() >= 2 {
        source_footer(&String::from_utf8_lossy(&tail[1..tail.len() - 1]))
    } else {
        Ok(None)
    };
    Some((history, future))
}

fn footer_digit(c: char) -> bool {
    unicode_general_category::get_general_category(c)
        == unicode_general_category::GeneralCategory::DecimalNumber
}

fn footer_take<'a>(text: &mut &'a str, stop: impl Fn(char) -> bool) -> &'a str {
    let end = text.find(stop).unwrap_or(text.len());
    let (value, rest) = text.split_at(end);
    *text = rest;
    value
}

fn footer_name<'a>(text: &mut &'a str) -> &'a str {
    if let Some(rest) = text.strip_prefix('<') {
        *text = rest;
        let name = footer_take(text, |c| c == '>');
        *text = text.strip_prefix('>').unwrap_or(text);
        name
    } else {
        footer_take(text, |c| footer_digit(c) || matches!(c, '+' | '-' | ','))
    }
}

// TZif_ParseOffsetString: integer hours first, otherwise invariant TimeSpan g.
// The footer's offset tokenizer excludes '.', but transition times admit the
// fractional-second g form. Values here are ticks, not decimal floating point.
fn footer_span(text: &str) -> Result<Option<i64>, ()> {
    let text = text.trim();
    let (sign, text) = if let Some(rest) = text.strip_prefix('-') {
        (-1, rest)
    } else {
        (1, text.strip_prefix('+').unwrap_or(text))
    };
    if let Ok(hours) = text.parse::<i32>() {
        return i64::from(hours)
            .checked_mul(3600)
            .and_then(|seconds| seconds.checked_mul(TICKS_SECOND))
            .and_then(|ticks| ticks.checked_mul(sign))
            .map(Some)
            .ok_or(());
    }
    match footer_span_g(text) {
        Some(ticks) => ticks.checked_mul(sign).map(Some).ok_or(()),
        None => Ok(None),
    }
}

fn footer_span_g(text: &str) -> Option<i64> {
    let text = text.trim();
    let (sign, text) = text.strip_prefix('-').map_or((1, text), |rest| (-1, rest));
    let parts: Vec<_> = text.split(':').collect();
    if !(2..=4).contains(&parts.len()) {
        return None;
    }
    let mut numbers = Vec::new();
    let mut fractional = false;
    let mut fraction = 0i128;
    for (index, part) in parts.iter().enumerate() {
        let (whole, subsecond) = part
            .split_once('.')
            .map_or((*part, None), |(a, b)| (a, Some(b)));
        if !whole.bytes().all(|b| b.is_ascii_digit()) || (whole.is_empty() && subsecond.is_none()) {
            return None;
        }
        numbers.push(if whole.is_empty() {
            0
        } else {
            i64::from(whole.parse::<i32>().ok()?)
        });
        if let Some(digits) = subsecond {
            if index != parts.len() - 1
                || parts.len() < 3
                || digits.is_empty()
                || !digits.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            let number = digits.parse::<i32>().ok()?;
            if number > 268_435_455 || (!digits.starts_with('0') && number > 9_999_999) {
                return None;
            }
            let zeroes = digits.bytes().take_while(|b| *b == b'0').count();
            fraction = if number == 0 || zeroes > 7 {
                0
            } else if digits.len() <= 7 {
                i128::from(number) * 10i128.pow(7 - digits.len() as u32)
            } else {
                let power = 10i128.pow((digits.len() - 7) as u32);
                (i128::from(number) + power / 2) / power
            };
            // ParseTimeOfDay later drops this fraction, but g must first pass
            // its TimeSpan tick bound. The h:m:.f shorthand is also reachable.
            fractional = true;
        }
    }
    let ticks = |days: i64, hour: i64, minute: i64, second: i64| {
        if days > 10_675_199 || hour > 23 || minute > 59 || second > 59 {
            return None;
        }
        let seconds = i128::from(days) * 86400 + i128::from(hour * 3600 + minute * 60 + second);
        i64::try_from((seconds * i128::from(TICKS_SECOND) + fraction) * sign).ok()
    };
    match numbers.as_slice() {
        [hour, minute] => ticks(0, *hour, *minute, 0),
        [a, b, c] => ticks(0, *a, *b, *c).or_else(|| {
            // In invariant localized g, the day/hour separator is ALSO ':'.
            // TimeSpanParse tries HMS first, then DHM (26:00:01 is 26d1m).
            (!fractional).then(|| ticks(*a, *b, *c, 0)).flatten()
        }),
        [days, hour, minute, second] => ticks(*days, *hour, *minute, *second),
        _ => None,
    }
}

fn footer_day(text: &str) -> Result<Option<tz::timezone::RuleDay>, ()> {
    use tz::timezone::{Julian1WithoutLeap, MonthWeekDay, RuleDay};
    if let Some(text) = text.strip_prefix('M') {
        let parts: Vec<_> = text.split('.').collect();
        if parts.len() != 3 {
            return Err(());
        }
        let number = |part: &str| {
            part.trim()
                .parse::<i32>()
                .ok()
                .and_then(|n| u8::try_from(n).ok())
                .ok_or(())
        };
        return MonthWeekDay::new(number(parts[0])?, number(parts[1])?, number(parts[2])?)
            .map(|day| Some(RuleDay::MonthWeekDay(day)))
            .map_err(|_| ());
    }
    let nominal = if let Some(text) = text.strip_prefix('J') {
        let digits = text.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return Err(());
        }
        let number = text[..digits].bytes().fold(0i32, |n, b| {
            n.wrapping_mul(10).wrapping_add(i32::from(b - b'0'))
        });
        if !(1..=365).contains(&number) {
            return Err(());
        }
        number
    } else {
        let Ok(number) = text.trim().parse::<i32>() else {
            return Ok(None);
        };
        if number >= 59 {
            return Ok(None);
        }
        number.checked_add(1).ok_or(())?
    };
    Julian1WithoutLeap::new(nominal.try_into().map_err(|_| ())?)
        .map(|day| Some(RuleDay::Julian1WithoutLeap(day)))
        .map_err(|_| ())
}

fn footer_transition(text: &str) -> Result<Option<(tz::timezone::RuleDay, i64)>, ()> {
    let (day, time) = text.split_once('/').unwrap_or((text, ""));
    let Some(day) = footer_day(day)? else {
        return Ok(None);
    };
    // ParseTimeOfDay rebuilds Hours/Minutes/Seconds: days AND fractional
    // seconds disappear before it constructs the TransitionTime clock.
    let ticks = (footer_span(time)?.unwrap_or(2 * 3600 * TICKS_SECOND) / TICKS_SECOND
        * TICKS_SECOND)
        .rem_euclid(TICKS_DAY);
    Ok(Some((day, ticks)))
}

fn source_footer(mut text: &str) -> Result<Option<FutureRule>, ()> {
    let name = footer_name(&mut text);
    let offset = footer_take(&mut text, |c| {
        !footer_digit(c) && !matches!(c, '+' | '-' | ':')
    });
    if name.is_empty() || offset.is_empty() {
        return Ok(None);
    }
    let Some(standard) = footer_span(offset)? else {
        return Ok(None);
    };
    let standard = standard.checked_neg().ok_or(())?;
    let daylight_name = footer_name(&mut text);
    if daylight_name.is_empty() {
        return Ok(Some(FutureRule::Fixed(standard / TICKS_SECOND)));
    }
    let offset = footer_take(&mut text, |c| {
        !footer_digit(c) && !matches!(c, '+' | '-' | ':')
    });
    let daylight = footer_span(offset)?
        .and_then(i64::checked_neg)
        .map(|ticks| ticks / TICKS_SECOND);
    let mut rules = text.strip_prefix(',').unwrap_or("").split(',');
    let start = footer_transition(rules.next().unwrap_or(""));
    let end = footer_transition(rules.next().unwrap_or(""));
    // Both source transition calls run before either null-result check.
    let (Some((start, start_ticks)), Some((end, end_ticks))) = (start?, end?) else {
        return Ok(None);
    };
    if start == end && start_ticks == end_ticks {
        return Err(());
    }
    Ok(Some(FutureRule::Alternate(FutureTransitions {
        standard: standard / TICKS_SECOND,
        daylight: daylight.unwrap_or(0),
        default_daylight: daylight.is_none(),
        start,
        start_ticks,
        end,
        end_ticks,
    })))
}

struct SelectedZone {
    bytes: Vec<u8>,
    id: String,
}

fn is_utc_alias(id: &str) -> bool {
    // TimeZoneInfo.Unix.IsUtcAlias: this is the selected ID, not a basename
    // or a canonicalized relative path ("./UTC" is not the alias "UTC").
    [
        "Etc/UTC",
        "Etc/UCT",
        "Etc/Universal",
        "Etc/Zulu",
        "UCT",
        "UTC",
        "Universal",
        "Zulu",
    ]
    .iter()
    .any(|alias| id.eq_ignore_ascii_case(alias))
}

fn normalized_absolute(path: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::path::Component;
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let mut normalized = std::path::PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            part => normalized.push(part.as_os_str()),
        }
    }
    Some(normalized)
}

fn directory_prefix(directory: &std::path::Path) -> String {
    let mut prefix = directory.to_string_lossy().into_owned();
    if !prefix.ends_with('/') {
        prefix.push('/');
    }
    prefix
}

fn source_zone_id(path: &std::path::Path, directory: &std::path::Path, bytes: &[u8]) -> String {
    let prefix = directory_prefix(directory);
    // FindTimeZoneIdUsingReadLink resolves the link lexically, not the selected
    // file's basename or a relative TZ ID. An outside link falls through to the
    // same byte-matching lookup as an ordinary absolute/unset TZ path.
    if let Ok(target) = std::fs::read_link(path) {
        let target = if target.is_absolute() {
            target
        } else {
            path.parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join(target)
        };
        if let Some(target) = normalized_absolute(&target) {
            if let Some(id) = target.to_string_lossy().strip_prefix(&prefix) {
                if !id.is_empty() {
                    return id.to_owned();
                }
            }
        }
    }
    // FindTimeZoneId uses the first byte-identical file, skipping localtime and
    // posixrules. The scan is needed only when no relative ID was supplied.
    let localtime = directory.join("localtime").to_string_lossy().into_owned();
    let posixrules = directory.join("posixrules").to_string_lossy().into_owned();
    let mut pending = std::collections::VecDeque::from([directory.to_owned()]);
    while let Some(folder) = pending.pop_front() {
        let Ok(entries) = std::fs::read_dir(folder) else {
            return "Local".into();
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return "Local".into();
            };
            let candidate = entry.path();
            let Ok(metadata) = std::fs::metadata(&candidate) else {
                continue;
            };
            if metadata.is_dir() {
                pending.push_back(candidate);
                continue;
            }
            let name = candidate.to_string_lossy();
            if name.eq_ignore_ascii_case(&localtime) || name.eq_ignore_ascii_case(&posixrules) {
                continue;
            }
            if metadata.len() == bytes.len() as u64
                && std::fs::read(&candidate).is_ok_and(|data| data == bytes)
            {
                return name.strip_prefix(&prefix).unwrap_or(&name).to_owned();
            }
        }
    }
    "Local".into()
}

fn source_zone_bytes() -> Option<SelectedZone> {
    let directory = std::env::var_os("TZDIR")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "/usr/share/zoneinfo".into());
    let read = |path: &std::path::Path, id: Option<&str>| {
        let bytes = std::fs::read(path).ok()?;
        let id = id
            .map(str::to_owned)
            .unwrap_or_else(|| source_zone_id(path, &directory, &bytes));
        Some(SelectedZone { bytes, id })
    };
    match std::env::var_os("TZ") {
        None => read(std::path::Path::new("/etc/localtime"), None)
            .or_else(|| read(&directory.join("localtime"), None)),
        Some(value) => {
            let value = value.to_string_lossy();
            let value = value.strip_prefix(':').unwrap_or(&value);
            if value.is_empty() {
                return None;
            }
            let path = std::path::Path::new(value);
            if path.is_absolute() {
                read(path, None)
            } else {
                read(&directory.join(path), Some(value))
            }
        }
    }
}

fn component_minutes(seconds: i64) -> i64 {
    if seconds % 60 != 0 {
        seconds % 86400 / 60 * 60
    } else {
        seconds
    }
}

fn normalized_offset_i64(offset: i64, base: i32) -> i32 {
    let base = i64::from(base);
    (base + component_minutes(offset - base)).clamp(-50400, 50400) as i32
}

fn normalized_offset(offset: i32, base: i32) -> i32 {
    normalized_offset_i64(i64::from(offset), base)
}

fn normalize_zone(
    zone: tz::TimeZone,
    future: Result<Option<FutureRule>, ()>,
    now: i64,
) -> Option<SourceZone> {
    use tz::timezone::{LocalTimeType, Transition};
    let raw = zone.as_ref();
    let types = raw.local_time_types();
    let transitions = raw.transitions();
    let mut base = 0;
    if transitions.is_empty() {
        for kind in types.iter().filter(|kind| !kind.is_dst()) {
            base = kind.ut_offset();
        }
    } else {
        for transition in transitions
            .iter()
            .take_while(|transition| transition.unix_leap_time() <= now)
        {
            let kind = types.get(transition.local_time_type_index())?;
            if !kind.is_dst() {
                base = kind.ut_offset();
            }
        }
    }
    base = component_minutes(i64::from(base)) as i32;
    if base.abs() > 50400 {
        return None;
    }
    // TimeZoneInfo does not create adjustment rules without transitions.
    let fixed_base = || {
        Some(SourceZone {
            history: tz::TimeZone::fixed(base).ok()?,
            future: None,
            base,
        })
    };
    if transitions.is_empty() || future.is_err() {
        // GetTimeZoneFromTzData retries an invalid adjustment rule with all DST
        // rules disabled, retaining the valid base offset (not UTC).
        return fixed_base();
    }
    let normalize = |kind: &LocalTimeType| {
        LocalTimeType::new(
            normalized_offset(kind.ut_offset(), base),
            kind.is_dst(),
            None,
        )
        .ok()
    };
    let early = types
        .iter()
        .find(|kind| !kind.is_dst())
        .or_else(|| types.first())?;
    let mut normalized_types = vec![normalize(early)?];
    for kind in types {
        normalized_types.push(normalize(kind)?);
    }
    let mut normalized_transitions = Vec::new();
    for transition in transitions {
        normalized_transitions.push(Transition::new(
            transition.unix_leap_time(),
            transition.local_time_type_index() + 1,
        ));
    }
    let future = match future.ok()? {
        Some(FutureRule::Fixed(offset)) => {
            FutureRule::Fixed(i64::from(normalized_offset_i64(offset, base)))
        }
        Some(FutureRule::Alternate(mut rule)) => {
            let base_delta = component_minutes(rule.standard - i64::from(base));
            let mut delta = if rule.default_daylight {
                3600
            } else {
                component_minutes(component_minutes(rule.daylight - i64::from(base)) - base_delta)
            };
            rule.standard = i64::from(base) + base_delta;
            // AdjustmentRule.CreateAdjustmentRule first recenters deltas by a
            // single day, then validates [-23h,+14h], then clamps the DST total.
            if delta > 43200 {
                delta -= 86400;
                rule.standard += 86400;
            } else if delta < -43200 {
                delta += 86400;
                rule.standard -= 86400;
            }
            if !(-82800..=50400).contains(&delta) {
                return fixed_base();
            }
            let total = rule.standard + delta;
            rule.standard += total.clamp(-50400, 50400) - total;
            rule.daylight = rule.standard + delta;
            FutureRule::Alternate(rule)
        }
        None => {
            let last = transitions.last()?;
            FutureRule::Fixed(i64::from(
                normalized_types[last.local_time_type_index() + 1].ut_offset(),
            ))
        }
    };
    Some(SourceZone {
        history: tz::TimeZone::new(normalized_transitions, normalized_types, vec![], None).ok()?,
        future: Some(future),
        base,
    })
}

fn source_rule_date(year: i32, day: &tz::timezone::RuleDay) -> Option<NaiveDate> {
    use tz::timezone::RuleDay;
    match day {
        RuleDay::Julian1WithoutLeap(day) => {
            let nominal = NaiveDate::from_yo_opt(2001, u32::from(day.get()))?;
            NaiveDate::from_ymd_opt(year, nominal.month(), nominal.day())
        }
        RuleDay::Julian0WithLeap(day) => NaiveDate::from_yo_opt(year, u32::from(day.get()) + 1),
        RuleDay::MonthWeekDay(day) => {
            let first = NaiveDate::from_ymd_opt(year, u32::from(day.month()), 1)?;
            let delta = (i64::from(day.week_day())
                - i64::from(first.weekday().num_days_from_sunday()))
            .rem_euclid(7);
            let mut candidate = first + Duration::days(delta + 7 * i64::from(day.week() - 1));
            if candidate.month() != first.month() {
                candidate -= Duration::days(7);
            }
            Some(candidate)
        }
    }
}

fn rule_time(year: i32, day: &tz::timezone::RuleDay, ticks: i64) -> Option<NaiveDateTime> {
    Some(source_rule_date(year, day)?.and_hms_opt(0, 0, 0)? + Duration::nanoseconds(ticks * 100))
}

fn rule_marker(day: &tz::timezone::RuleDay, ticks: i64) -> bool {
    use tz::timezone::RuleDay;
    // TransitionTime.Day is the placeholder 1 for every floating rule. Source
    // checks those fields, NOT the evaluated weekday date for this year.
    ticks < TICKS_SECOND
        && match day {
            RuleDay::Julian1WithoutLeap(day) => day.get() == 1,
            RuleDay::Julian0WithLeap(day) => day.get() == 0,
            RuleDay::MonthWeekDay(day) => day.month() == 1,
        }
}

fn with_year(time: NaiveDateTime, year: i32) -> Option<NaiveDateTime> {
    // DateTime.AddYears clamps February 29, unlike chrono::with_year.
    time.with_year(year)
        .or_else(|| time.with_day(28)?.with_year(year))
}

fn source_future_local_offset(zone: &SourceZone, time: NaiveDateTime) -> Option<Result<i32, ()>> {
    if !zone.future_active_at(time) {
        return None;
    }
    let rule = match zone.future? {
        FutureRule::Fixed(offset) => return Some(Ok(offset as i32)),
        FutureRule::Alternate(rule) => rule,
    };
    let mut start = rule_time(time.year(), &rule.start, rule.start_ticks)?;
    let mut end = rule_time(time.year(), &rule.end, rule.end_ticks)?;
    let delta = rule.daylight - rule.standard;
    if rule_marker(&rule.start, rule.start_ticks) {
        start = NaiveDate::from_ymd_opt(time.year(), 1, 1)?.and_hms_opt(0, 0, 0)?;
    } else if delta > 0 {
        start += Duration::seconds(delta);
    }
    if rule_marker(&rule.end, rule.end_ticks) {
        // Source constructs next Jan 1 BEFORE subtracting a tick, and its
        // ArgumentOutOfRangeException at year 9999 is caught by the formatter.
        if time.year() == 9999 {
            return Some(Err(()));
        }
        end = NaiveDate::from_ymd_opt(time.year() + 1, 1, 1)?.and_hms_opt(0, 0, 0)?
            - Duration::nanoseconds(100);
    } else if delta > 0 {
        end -= Duration::seconds(delta);
    }
    if !(1..=9999).contains(&start.year()) || !(1..=9999).contains(&end.year()) {
        return Some(Err(()));
    }
    end = with_year(end, start.year())?;
    let time = with_year(time, start.year())?;
    let dst = if start > end {
        time < end || time >= start
    } else {
        time >= start && time < end
    };
    Some(Ok(if dst { rule.daylight } else { rule.standard } as i32))
}

fn local_boundary_valid(time: NaiveDateTime, ticks: i64) -> Result<bool, ()> {
    use chrono::Timelike;
    use tz::datetime::FoundDateTimeKind;
    let zone = local_zone();
    let valid = |offset: i32| {
        offset.abs() <= 50400
            && (0..=MAX_TICKS).contains(&(ticks - i64::from(offset) * TICKS_SECOND))
    };
    if let Some(offset) = source_future_local_offset(zone, time) {
        return offset.map(valid);
    }
    if (MAX_OFFSET..=MAX_TICKS - MAX_OFFSET).contains(&ticks) {
        return Ok(true);
    }
    let found = tz::DateTime::find(
        time.year(),
        time.month() as u8,
        time.day() as u8,
        time.hour() as u8,
        time.minute() as u8,
        time.second() as u8,
        time.nanosecond(),
        zone.history.as_ref(),
    );
    let Ok(found) = found else {
        return Ok(false);
    };
    let mut candidates = Vec::new();
    for candidate in found.into_inner() {
        match candidate {
            FoundDateTimeKind::Normal(time) => candidates.push(*time.local_time_type()),
            FoundDateTimeKind::Skipped {
                before_transition,
                after_transition,
            } => {
                candidates.push(*before_transition.local_time_type());
                candidates.push(*after_transition.local_time_type());
            }
        }
    }
    // Source NoThrowOnInvalidTime and Unspecified-kind overlap resolution use
    // standard time (also when DST has a negative delta), not earlier/UTC.
    let Some(kind) = candidates
        .iter()
        .find(|kind| !kind.is_dst())
        .or_else(|| candidates.first())
    else {
        return Ok(false);
    };
    Ok(valid(kind.ut_offset()))
}

fn punctuation(input: &str, pos: usize) -> Option<usize> {
    if input[pos..].starts_with('\0') {
        return input[pos..]
            .chars()
            .all(|c| c == '\0')
            .then_some(input.len());
    }
    if !input[pos..].starts_with('#') {
        return None;
    }
    let mut hashes = 0;
    for c in input.chars() {
        if c == '#' {
            hashes += 1;
        } else if c == '\0' {
            if hashes != 2 {
                return None;
            }
        } else if !c.is_whitespace() && hashes != 1 {
            return None;
        }
        if hashes > 2 {
            return None;
        }
    }
    (hashes == 2).then_some(pos + 1)
}

fn lex(lexer: &mut Lexer<'_>, state: u8, raw: &mut Raw, parsed: &mut Parsed) -> Option<usize> {
    lexer.skip_space();
    if lexer.peek().is_none() {
        return Some(END);
    }
    if lexer.peek()?.is_ascii_digit() {
        if raw.numbers.len() == 3 {
            return None;
        }
        let (mut number, width) = lexer.number()?;
        if state == 19 && matches!(lexer.peek(), Some('.' | ',')) && lexer.rest().len() > 1 {
            lexer.fraction();
            raw.fraction = lexer.pending_fraction;
        }
        if matches!(state, 18 | 19) {
            lexer.handle_offset(parsed)?;
        }
        let before_separator = lexer.pos;
        let separator = lexer.separator();
        let year = width >= 3;
        if year && raw.year.replace(number).is_some() {
            return None;
        }
        let token = match separator {
            Separator::End => {
                if year {
                    YEAR_END
                } else {
                    NUM_END
                }
            }
            Separator::Space => {
                if year {
                    YEAR_SPACE
                } else {
                    NUM_SPACE
                }
            }
            Separator::Date => {
                if year {
                    YEAR_DATE
                } else {
                    NUM_DATE
                }
            }
            Separator::Offset => {
                let (date, space) = if year {
                    (YEAR_DATE, YEAR_SPACE)
                } else {
                    (NUM_DATE, NUM_SPACE)
                };
                if STATES[state as usize][date] == 20 && STATES[state as usize][space] > 20 {
                    lexer.pos = before_separator;
                    space
                } else {
                    date
                }
            }
            Separator::Time if !year => NUM_TIME,
            Separator::Meridiem(pm) => {
                if raw.pm.replace(pm).is_some() {
                    return None;
                }
                if !year && state == 4 {
                    parsed.terminal(21, raw)?;
                }
                if matches!(state, 18 | 19) {
                    lexer.handle_offset(parsed)?;
                }
                if year { YEAR_SPACE } else { NUM_AMPM }
            }
            Separator::Suffix(field) => {
                if parsed.suffix_fields & (1 << field) != 0 {
                    return None;
                }
                if field == 0 && !year {
                    number = expand_year(number);
                }
                parsed.suffix_fields |= 1 << field;
                parsed.fields[field] = Some(number);
                if field == 0 {
                    raw.year = Some(number);
                }
                if field == 1 {
                    raw.month = Some(number);
                }
                return Some(if field < 3 { DATE_SUFFIX } else { TIME_SUFFIX });
            }
            Separator::Local if !year => LOCAL_MARK,
            _ => return None,
        };
        if !year {
            raw.numbers.push(number);
        }
        return Some(token);
    }
    if lexer
        .peek()
        .is_some_and(|c| ('\u{0590}'..='\u{05ff}').contains(&c) && is_letter(c))
    {
        // The fixed reference runtime uses ICU: Tokenize's Hebrew-number guard
        // is !GlobalizationMode.Invariant, not a Hebrew-culture/calendar guard.
        let number = lexer.hebrew_number()?;
        let before_separator = lexer.pos;
        let separator = lexer.separator();
        if number >= 100 {
            if raw.year.replace(number).is_some() {
                return None;
            }
            return Some(match separator {
                Separator::End => YEAR_END,
                Separator::Space => YEAR_SPACE,
                Separator::Offset if STATES[state as usize][YEAR_SPACE] > 20 => {
                    lexer.pos = before_separator;
                    YEAR_SPACE
                }
                _ => return None,
            });
        }
        if raw.numbers.len() == 3 {
            return None;
        }
        raw.numbers.push(number);
        return Some(match separator {
            Separator::End => NUM_END,
            Separator::Space | Separator::Date => NUM_DATE,
            Separator::Offset => {
                if STATES[state as usize][NUM_DATE] == 20 && STATES[state as usize][NUM_SPACE] > 20
                {
                    lexer.pos = before_separator;
                    NUM_SPACE
                } else {
                    NUM_DATE
                }
            }
            _ => return None,
        });
    }
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    for (index, month) in MONTHS.iter().enumerate() {
        if lexer.word(month) || lexer.word(&month[..3]) {
            if raw.month.replace(index as i32 + 1).is_some() {
                return None;
            }
            let before_separator = lexer.pos;
            return Some(match lexer.separator() {
                Separator::End => MONTH_END,
                Separator::Space => MONTH_SPACE,
                Separator::Date => MONTH_DATE,
                Separator::Offset => {
                    if STATES[state as usize][MONTH_DATE] == 20
                        && STATES[state as usize][MONTH_SPACE] > 20
                    {
                        lexer.pos = before_separator;
                        MONTH_SPACE
                    } else {
                        MONTH_DATE
                    }
                }
                _ => return None,
            });
        }
    }
    const WEEKDAYS: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    for (index, weekday) in WEEKDAYS.iter().enumerate() {
        if lexer.word(weekday) || lexer.word(&weekday[..3]) {
            if raw.weekday.replace(index as u32).is_some() {
                return None;
            }
            return Some(WEEKDAY);
        }
    }
    if lexer.word("GMT") || lexer.word("Z") {
        if parsed.offset.replace(0).is_some() {
            return None;
        }
        return Some(ZONE);
    }
    if lexer.word("AM") || lexer.word("PM") {
        let pm = lexer.input[..lexer.pos].ends_with(['m', 'M'])
            && lexer.input.as_bytes()[lexer.pos - 2].eq_ignore_ascii_case(&b'p');
        if raw.pm.replace(pm).is_some() {
            return None;
        }
        return Some(UNKNOWN);
    }
    if lexer.word("A.D.") || lexer.word("AD") {
        return Some(ERA);
    }
    if lexer.consume('.') || lexer.consume(',') {
        return Some(UNKNOWN);
    }
    if matches!(lexer.peek(), Some('+' | '-')) && parsed.offset.is_none() {
        parsed.offset = Some(lexer.offset()?);
        return Some(UNKNOWN);
    }
    lexer.pos = punctuation(lexer.input, lexer.pos)?;
    Some(UNKNOWN)
}

fn iso(lexer: &mut Lexer<'_>, raw: &mut Raw, parsed: &mut Parsed) -> Option<Result<bool, ()>> {
    let mut clock = [0; 3];
    let mut have_seconds = false;
    for (index, field) in clock.iter_mut().enumerate() {
        lexer.skip_space();
        let (n, width) = lexer.number()?;
        if width != 2 {
            return None;
        }
        *field = n;
        if index == 0 {
            lexer.skip_space();
            if !lexer.consume(':') {
                return None;
            }
        } else if index == 1 {
            lexer.skip_space();
            if !lexer.consume(':') {
                break;
            }
            have_seconds = true;
        }
    }
    if have_seconds && matches!(lexer.peek(), Some('.' | ',')) {
        if !lexer.fraction() {
            return None;
        }
        raw.fraction = lexer.pending_fraction;
    }
    lexer.skip_space();
    if matches!(lexer.peek(), Some('+' | '-')) {
        parsed.offset = Some(lexer.offset()?);
    } else if lexer.consume('Z') || lexer.consume('z') {
        parsed.offset = Some(0);
    }
    lexer.skip_space();
    if lexer.peek() == Some('#') {
        lexer.pos = punctuation(lexer.input, lexer.pos)?;
        lexer.skip_space();
    }
    if lexer.peek() == Some('\0') {
        lexer.pos = punctuation(lexer.input, lexer.pos)?;
    }
    if lexer.peek().is_some() {
        return None;
    }
    parsed.fields = [
        raw.year,
        raw.numbers.first().copied(),
        raw.numbers.get(1).copied(),
        Some(clock[0]),
        Some(clock[1]),
        Some(clock[2]),
    ];
    parsed.finish(raw, true)
}

pub(super) fn classify(value: &str) -> Result<bool, ()> {
    parse(value, Parsed::default())
}

#[cfg(test)]
pub(super) fn matches(value: &str) -> bool {
    classify(value).unwrap_or(false)
}

#[cfg(test)]
pub(super) fn clock_offset_at(utc: chrono::DateTime<Utc>) -> Option<i32> {
    local_zone().at_utc(utc.timestamp())
}

#[cfg(test)]
pub(super) fn future_or_fixed_offset_at(time: NaiveDateTime) -> Option<Result<i32, ()>> {
    let zone = local_zone();
    source_future_local_offset(zone, time).or_else(|| {
        zone.history
            .as_ref()
            .transitions()
            .is_empty()
            .then(|| Ok(zone.history.as_ref().local_time_types()[0].ut_offset()))
    })
}

#[cfg(test)]
pub(super) fn matches_at(
    value: &str,
    utc: chrono::DateTime<Utc>,
    local_today: NaiveDate,
) -> Result<bool, ()> {
    parse(
        value,
        Parsed {
            clock: Some((utc, local_today)),
            ..Parsed::default()
        },
    )
}

fn parse(value: &str, mut parsed: Parsed) -> Result<bool, ()> {
    let mut lexer = Lexer::new(value);
    let mut raw = Raw::default();
    let mut state = 0;
    let mut terminal = false;
    loop {
        let Some(token) = lex(&mut lexer, state, &mut raw, &mut parsed) else {
            return Ok(false);
        };
        if token == LOCAL_MARK {
            return if matches!(state, 12 | 13) {
                iso(&mut lexer, &mut raw, &mut parsed).unwrap_or(Ok(false))
            } else {
                Ok(false)
            };
        }
        if token != UNKNOWN {
            state = STATES[state as usize][token];
            if state == 20 {
                return Ok(false);
            }
            if state > 20 {
                if parsed.terminal(state, &mut raw).is_none() {
                    return Ok(false);
                }
                terminal = true;
                state = 0;
            }
        }
        if matches!(token, END | NUM_END | MONTH_END) {
            break;
        }
    }
    if terminal {
        parsed.finish(&raw, false).unwrap_or(Ok(false))
    } else {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn private_clock_preserves_year_and_supplied_offset_defaults() {
        let utc = Utc.with_ymd_and_hms(2019, 12, 31, 12, 0, 0).unwrap();
        let local = NaiveDate::from_ymd_opt(2019, 12, 31).unwrap();
        assert_eq!(matches_at("Wed 13PM +14", utc, local), Ok(true));
        assert_eq!(matches_at("Tue 13PM +14", utc, local), Ok(false));
        let leap = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(matches_at("Feb 29", leap, leap.date_naive()), Ok(true));
        assert_ne!(future_or_fixed_offset_at(leap.naive_utc()), Some(Err(())));
        assert!(clock_offset_at(leap).is_some());
        assert_eq!(matches_at("Feb 29", utc, local), Ok(false));
        assert!(matches("13PM"));
    }
}
