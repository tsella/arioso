//! Turning digits and symbols into words before phonemization.
//!
//! English only. Every other language returns its input unchanged until it has
//! rules of its own.

const ONES: [&str; 20] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];

const TENS: [&str; 10] = [
    "", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
];

/// Abbreviations that are safe to expand in any sentence.
const ABBREVIATIONS: [(&str, &str); 12] = [
    ("dr.", "doctor"),
    ("mr.", "mister"),
    ("mrs.", "misses"),
    ("ms.", "miss"),
    ("st.", "saint"),
    ("vs.", "versus"),
    ("etc.", "et cetera"),
    ("e.g.", "for example"),
    ("i.e.", "that is"),
    ("approx.", "approximately"),
    ("dept.", "department"),
    ("inc.", "incorporated"),
];

/// How text is expanded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Say the part of the day after a clock time: "seven forty five in the
    /// evening". A written `am` or `pm` decides it; otherwise the hour does.
    pub time_of_day: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { time_of_day: true }
    }
}

/// Expand numbers, currency, times, percentages, and common abbreviations,
/// with the default [`Options`].
#[must_use]
pub fn normalize(text: &str) -> String {
    normalize_with(text, Options::default())
}

/// Expand with explicit [`Options`].
#[must_use]
pub fn normalize_with(text: &str, options: Options) -> String {
    let expanded = expand_abbreviations(text);
    let chars: Vec<char> = expanded.chars().collect();
    let mut out = String::with_capacity(expanded.len() * 2);
    let mut index = 0;

    while index < chars.len() {
        let ch = chars[index];

        if ch == '$' {
            if let Some(scanned) = scan_number(&chars, index + 1) {
                out.push_str(&currency(scanned.whole, &scanned.fraction));
                index = scanned.end;
                continue;
            }
        }

        if ch.is_ascii_digit() {
            if let Some((hour, minute, next)) = scan_time(&chars, index) {
                // A written marker decides the period and is consumed, so
                // "7:45 pm" does not leave "p m" behind.
                let (hour, next) = match scan_marker(&chars, next) {
                    Some((pm, after)) => {
                        let hour = if pm { hour % 12 + 12 } else { hour % 12 };
                        (hour, after)
                    }
                    None => (hour, next),
                };
                out.push_str(&clock(hour, minute, options));
                index = next;
                continue;
            }
            if let Some(scanned) = scan_number(&chars, index) {
                if let Some((suffix, after)) = scan_ordinal_suffix(&chars, scanned.end) {
                    out.push_str(&ordinal(scanned.whole, suffix));
                    index = after;
                    continue;
                }
                if chars.get(scanned.end) == Some(&'%') {
                    out.push_str(&number(&scanned));
                    out.push_str(" percent");
                    index = scanned.end + 1;
                    continue;
                }
                // A bare hour followed by a marker is a time: "7 pm", "9pm".
                // Without this, "9pm" came out as one unpronounceable token.
                if scanned.fraction.is_empty()
                    && !scanned.separated
                    && (1..=12).contains(&scanned.whole)
                {
                    if let Some((pm, after)) = scan_marker(&chars, scanned.end) {
                        let hour = if pm {
                            scanned.whole % 12 + 12
                        } else {
                            scanned.whole % 12
                        };
                        out.push_str(&clock(hour, 0, options));
                        index = after;
                        continue;
                    }
                }
                out.push_str(&number(&scanned));
                index = scanned.end;
                continue;
            }
        }

        out.push(ch);
        index += 1;
    }

    out
}

/// Expand the abbreviations in [`ABBREVIATIONS`], keeping the source capital.
fn expand_abbreviations(text: &str) -> String {
    let lower = text.to_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;

    while index < text.len() {
        let rest = &lower[index..];
        let found = ABBREVIATIONS
            .iter()
            .find(|(short, _)| rest.starts_with(short))
            .filter(|_| {
                // Only at a word start, so "address." does not match "dr.".
                index == 0
                    || !text[..index]
                        .chars()
                        .next_back()
                        .is_some_and(|previous| previous.is_alphanumeric())
            });

        if let Some((short, long)) = found {
            let source = &text[index..index + short.len()];
            if source.chars().next().is_some_and(char::is_uppercase) {
                let mut chars = long.chars();
                if let Some(first) = chars.next() {
                    out.extend(first.to_uppercase());
                    out.push_str(chars.as_str());
                }
            } else {
                out.push_str(long);
            }
            index += short.len();
            continue;
        }

        let ch = text[index..]
            .chars()
            .next()
            .expect("index is on a boundary");
        out.push(ch);
        index += ch.len_utf8();
    }

    out
}

/// A number as written: the whole part, and the fraction digits if any.
struct Scanned {
    whole: u64,
    fraction: Vec<u64>,
    digits: usize,
    /// A thousands separator was present, so this is a quantity, not a year.
    separated: bool,
    end: usize,
}

/// Read digits, optional thousands separators, and an optional decimal part.
fn scan_number(chars: &[char], start: usize) -> Option<Scanned> {
    let mut index = start;
    let mut whole = 0u64;
    let mut digits = 0;
    let mut separated = false;

    while let Some(&ch) = chars.get(index) {
        if ch.is_ascii_digit() {
            whole = whole
                .checked_mul(10)?
                .checked_add(u64::from(ch as u8 - b'0'))?;
            digits += 1;
            index += 1;
        } else if ch == ',' && digits > 0 {
            separated = true;
            index += 1;
        } else {
            break;
        }
    }
    if digits == 0 {
        return None;
    }

    let mut fraction = Vec::new();
    if chars.get(index) == Some(&'.') && chars.get(index + 1).is_some_and(char::is_ascii_digit) {
        let mut after = index + 1;
        while let Some(&ch) = chars.get(after) {
            if !ch.is_ascii_digit() {
                break;
            }
            fraction.push(u64::from(ch as u8 - b'0'));
            after += 1;
        }
        index = after;
    }

    Some(Scanned {
        whole,
        fraction,
        digits,
        separated,
        end: index,
    })
}

/// Read `H:MM` or `HH:MM` and return hours, minutes, and the index after.
fn scan_time(chars: &[char], start: usize) -> Option<(u64, u64, usize)> {
    let mut index = start;
    let mut hour = 0u64;
    let mut hour_digits = 0;
    while let Some(&ch) = chars.get(index) {
        if ch.is_ascii_digit() && hour_digits < 2 {
            hour = hour * 10 + u64::from(ch as u8 - b'0');
            hour_digits += 1;
            index += 1;
        } else {
            break;
        }
    }
    if hour_digits == 0 || chars.get(index) != Some(&':') {
        return None;
    }
    let mut minute = 0u64;
    let mut minute_digits = 0;
    let mut after = index + 1;
    while let Some(&ch) = chars.get(after) {
        if ch.is_ascii_digit() && minute_digits < 2 {
            minute = minute * 10 + u64::from(ch as u8 - b'0');
            minute_digits += 1;
            after += 1;
        } else {
            break;
        }
    }
    if minute_digits != 2 || hour > 23 || minute > 59 {
        return None;
    }
    Some((hour, minute, after))
}

/// Read an ordinal suffix and return it with the index after.
fn scan_ordinal_suffix(chars: &[char], start: usize) -> Option<(&'static str, usize)> {
    let rest: String = chars.get(start..start + 2)?.iter().collect();
    let suffix = match rest.to_lowercase().as_str() {
        "st" => "st",
        "nd" => "nd",
        "rd" => "rd",
        "th" => "th",
        _ => return None,
    };
    Some((suffix, start + 2))
}

/// Words for a scanned number, choosing the year reading for four-digit years.
fn number(scanned: &Scanned) -> String {
    if !scanned.fraction.is_empty() {
        let mut text = cardinal(scanned.whole);
        text.push_str(" point");
        for digit in &scanned.fraction {
            text.push(' ');
            text.push_str(ONES[*digit as usize]);
        }
        return text;
    }

    if scanned.digits == 4 && !scanned.separated && (1100..=2099).contains(&scanned.whole) {
        let high = scanned.whole / 100;
        let low = scanned.whole % 100;
        if low == 0 {
            return format!("{} hundred", cardinal(high));
        }
        return format!("{} {}", cardinal(high), cardinal(low));
    }

    cardinal(scanned.whole)
}

/// Words for a whole number below one trillion.
#[must_use]
pub fn cardinal(value: u64) -> String {
    if value == 0 {
        return ONES[0].to_owned();
    }

    let mut parts: Vec<String> = Vec::new();
    let mut rest = value;
    for (divisor, name) in [
        (1_000_000_000u64, "billion"),
        (1_000_000, "million"),
        (1_000, "thousand"),
    ] {
        if rest >= divisor {
            parts.push(format!("{} {name}", cardinal(rest / divisor)));
            rest %= divisor;
        }
    }

    if rest > 0 {
        parts.push(below_thousand(rest));
    }
    parts.join(" ")
}

fn below_thousand(value: u64) -> String {
    let hundreds = value / 100;
    let rest = value % 100;
    let mut parts: Vec<String> = Vec::new();

    if hundreds > 0 {
        parts.push(format!("{} hundred", ONES[hundreds as usize]));
    }
    if rest > 0 {
        if rest < 20 {
            parts.push(ONES[rest as usize].to_owned());
        } else {
            let tens = (rest / 10) as usize;
            let ones = (rest % 10) as usize;
            if ones == 0 {
                parts.push(TENS[tens].to_owned());
            } else {
                parts.push(format!("{} {}", TENS[tens], ONES[ones]));
            }
        }
    }
    parts.join(" ")
}

/// The ordinal form of a value: `3` with `rd` becomes `third`.
#[must_use]
pub fn ordinal(value: u64, _suffix: &str) -> String {
    let words = cardinal(value);
    let (head, last) = match words.rsplit_once(' ') {
        Some((head, last)) => (Some(head), last),
        None => (None, words.as_str()),
    };

    let irregular = match last {
        "one" => Some("first"),
        "two" => Some("second"),
        "three" => Some("third"),
        "five" => Some("fifth"),
        "eight" => Some("eighth"),
        "nine" => Some("ninth"),
        "twelve" => Some("twelfth"),
        _ => None,
    };

    let converted = irregular.map_or_else(
        || match last.strip_suffix('y') {
            Some(stem) => format!("{stem}ieth"),
            None => format!("{last}th"),
        },
        str::to_owned,
    );

    match head {
        Some(head) => format!("{head} {converted}"),
        None => converted,
    }
}

/// Dollars and cents.
fn currency(dollars: u64, fraction: &[u64]) -> String {
    let cents = fraction.first().copied().unwrap_or(0) * 10 + fraction.get(1).copied().unwrap_or(0);
    match (dollars, cents) {
        (0, 0) => "zero dollars".to_owned(),
        (0, cents) => format!("{} cents", cardinal(cents)),
        (1, 0) => "one dollar".to_owned(),
        (dollars, 0) => format!("{} dollars", cardinal(dollars)),
        (dollars, cents) => format!(
            "{} {} and {} cent{}",
            cardinal(dollars),
            if dollars == 1 { "dollar" } else { "dollars" },
            cardinal(cents),
            if cents == 1 { "" } else { "s" }
        ),
    }
}

/// The clock reading of an hour and minute pair.
fn clock(hour: u64, minute: u64, options: Options) -> String {
    let twelve = if hour % 12 == 0 { 12 } else { hour % 12 };
    let mut spoken = match minute {
        0 => format!("{} o'clock", cardinal(twelve)),
        minute if minute < 10 => format!("{} oh {}", cardinal(twelve), cardinal(minute)),
        minute => format!("{} {}", cardinal(twelve), cardinal(minute)),
    };
    if options.time_of_day {
        spoken.push_str(period(hour));
    }
    spoken
}

/// The part of the day, from the hour on a 24-hour clock.
fn period(hour: u64) -> &'static str {
    match hour {
        0..=11 => " in the morning",
        12..=17 => " in the afternoon",
        _ => " in the evening",
    }
}

/// Read an `am` or `pm` marker after a time.
///
/// Accepts `am`, `pm`, `a.m.`, and `p.m.`, with or without a space before, in
/// any case. Returns whether it was `pm`, and the index after the marker.
fn scan_marker(chars: &[char], start: usize) -> Option<(bool, usize)> {
    let mut index = start;
    while chars.get(index).is_some_and(|ch| ch.is_whitespace()) {
        index += 1;
    }

    // a, am, a.m, a.m. and the same with p. The closing dot is optional, so
    // "7:45 P.M" at the end of a line still counts.
    let pm = match chars.get(index)?.to_ascii_lowercase() {
        'a' => false,
        'p' => true,
        _ => return None,
    };
    let mut cursor = index + 1;
    if chars.get(cursor) == Some(&'.') {
        cursor += 1;
    }
    if chars
        .get(cursor)
        .is_none_or(|ch| ch.to_ascii_lowercase() != 'm')
    {
        return None;
    }
    cursor += 1;
    if chars.get(cursor) == Some(&'.') {
        cursor += 1;
    }

    // "7:45 amps" is a word, not a marker.
    if chars.get(cursor).is_some_and(|ch| ch.is_alphanumeric()) {
        return None;
    }
    Some((pm, cursor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cardinals() {
        assert_eq!(cardinal(0), "zero");
        assert_eq!(cardinal(7), "seven");
        assert_eq!(cardinal(13), "thirteen");
        assert_eq!(cardinal(21), "twenty one");
        assert_eq!(cardinal(100), "one hundred");
        assert_eq!(cardinal(101), "one hundred one");
        assert_eq!(cardinal(999), "nine hundred ninety nine");
        assert_eq!(cardinal(1000), "one thousand");
        assert_eq!(cardinal(1_000_001), "one million one");
    }

    #[test]
    fn currency() {
        assert_eq!(normalize("$3.99"), "three dollars and ninety nine cents");
        assert_eq!(normalize("$1"), "one dollar");
        assert_eq!(normalize("$0.50"), "fifty cents");
        assert_eq!(normalize("$1.01"), "one dollar and one cent");
    }

    #[test]
    fn ordinals() {
        assert_eq!(normalize("1st"), "first");
        assert_eq!(normalize("2nd"), "second");
        assert_eq!(normalize("3rd"), "third");
        assert_eq!(normalize("4th"), "fourth");
        assert_eq!(normalize("11th"), "eleventh");
        assert_eq!(normalize("12th"), "twelfth");
        assert_eq!(normalize("20th"), "twentieth");
        assert_eq!(normalize("21st"), "twenty first");
        assert_eq!(normalize("100th"), "one hundredth");
    }

    #[test]
    fn the_part_of_the_day_follows_a_time() {
        assert_eq!(normalize("7:45"), "seven forty five in the morning");
        assert_eq!(normalize("19:45"), "seven forty five in the evening");
        assert_eq!(normalize("12:30"), "twelve thirty in the afternoon");
        assert_eq!(normalize("00:30"), "twelve thirty in the morning");
        assert_eq!(normalize("17:59"), "five fifty nine in the afternoon");
        assert_eq!(normalize("18:00"), "six o'clock in the evening");
        assert_eq!(normalize("23:59"), "eleven fifty nine in the evening");
    }

    #[test]
    fn without_the_option_the_hour_stands_alone() {
        let options = Options { time_of_day: false };
        assert_eq!(normalize_with("19:45", options), "seven forty five");
        assert_eq!(normalize_with("07:45", options), "seven forty five");
    }

    #[test]
    fn a_written_marker_wins_and_is_consumed() {
        assert_eq!(normalize("7:45 pm"), "seven forty five in the evening");
        assert_eq!(normalize("7:45pm"), "seven forty five in the evening");
        assert_eq!(normalize("7:45 a.m."), "seven forty five in the morning");
        assert_eq!(normalize("12:30 am"), "twelve thirty in the morning");
        assert_eq!(normalize("12:30 pm"), "twelve thirty in the afternoon");
        assert_eq!(normalize("9:05 P.M."), "nine oh five in the evening");
        assert_eq!(normalize("9:05 P.M"), "nine oh five in the evening");
        assert_eq!(normalize("9:05 p.m"), "nine oh five in the evening");
        assert_eq!(normalize("9:05 aM"), "nine oh five in the morning");
    }

    #[test]
    fn a_bare_hour_with_a_marker_is_a_time() {
        assert_eq!(normalize("at 7 pm"), "at seven o'clock in the evening");
        assert_eq!(normalize("9pm"), "nine o'clock in the evening");
        assert_eq!(
            normalize("the 3 pm train"),
            "the three o'clock in the afternoon train"
        );
        assert_eq!(
            normalize("meet at 12 am"),
            "meet at twelve o'clock in the morning"
        );
        assert_eq!(
            normalize("meet at 12 pm"),
            "meet at twelve o'clock in the afternoon"
        );
    }

    #[test]
    fn a_number_that_is_not_hour_shaped_keeps_its_marker() {
        // Not 1 to 12, or carrying a separator, so it is a quantity.
        assert_eq!(normalize("2026 pm"), "twenty twenty six pm");
        assert_eq!(
            normalize("1,234 pm"),
            "one thousand two hundred thirty four pm"
        );
        assert_eq!(normalize("3.5 pm"), "three point five pm");
    }

    #[test]
    fn a_word_that_starts_like_a_marker_is_not_one() {
        assert_eq!(
            normalize("7:45 amps"),
            "seven forty five in the morning amps"
        );
    }

    #[test]
    fn times() {
        assert_eq!(normalize("14:30"), "two thirty in the afternoon");
        assert_eq!(normalize("14:00"), "two o'clock in the afternoon");
        assert_eq!(normalize("14:05"), "two oh five in the afternoon");
        assert_eq!(normalize("00:15"), "twelve fifteen in the morning");
        assert_eq!(normalize("9:41"), "nine forty one in the morning");
    }

    #[test]
    fn percentages() {
        assert_eq!(normalize("12%"), "twelve percent");
        assert_eq!(normalize("12.5%"), "twelve point five percent");
    }

    #[test]
    fn years() {
        assert_eq!(normalize("2026"), "twenty twenty six");
        assert_eq!(normalize("1999"), "nineteen ninety nine");
        assert_eq!(normalize("1900"), "nineteen hundred");
    }

    #[test]
    fn a_four_digit_number_that_is_not_a_year_is_a_cardinal() {
        assert_eq!(normalize("1,234"), "one thousand two hundred thirty four");
        assert_eq!(normalize("4321"), "four thousand three hundred twenty one");
    }

    #[test]
    fn decimals() {
        assert_eq!(normalize("3.5"), "three point five");
    }

    #[test]
    fn abbreviations_keep_their_capital() {
        assert_eq!(normalize("Dr. Smith arrived"), "Doctor Smith arrived");
        assert_eq!(normalize("and so on, etc."), "and so on, et cetera");
        assert_eq!(normalize("apples vs. pears"), "apples versus pears");
    }

    #[test]
    fn a_word_that_merely_contains_an_abbreviation_is_left_alone() {
        assert_eq!(normalize("the address."), "the address.");
        assert_eq!(normalize("hundreds."), "hundreds.");
    }

    #[test]
    fn plain_words_pass_through() {
        assert_eq!(normalize("hello world"), "hello world");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn numbers_inside_a_sentence_keep_their_neighbours() {
        assert_eq!(
            normalize("it costs $3.99 today"),
            "it costs three dollars and ninety nine cents today"
        );
        assert_eq!(
            normalize("call at 14:30 sharp"),
            "call at two thirty in the afternoon sharp"
        );
    }
}
