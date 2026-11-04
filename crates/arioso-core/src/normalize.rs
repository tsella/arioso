//! Turning digits and symbols into words before phonemization.
//!
//! English only. Every other language returns its input unchanged until it has
//! rules of its own.

use std::borrow::Cow;

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

/// A unit written straight after a number, and how it is said.
struct Unit {
    token: &'static str,
    singular: &'static str,
    plural: &'static str,
    /// True when the letters must match case. Only for a token that is also something
    /// else spelled the same way: `5G` is a network and `5g` is five grams.
    exact_case: bool,
}

/// Units that may follow a number with no space: `5kg`, `190cm`, `60mph`.
///
/// A space is not accepted, because `5 in the morning` and `2 m of rope` are the same
/// shape and only one of them is a unit.
const UNITS: [Unit; 42] = [
    Unit {
        token: "km/h",
        singular: "kilometer per hour",
        plural: "kilometers per hour",
        exact_case: false,
    },
    Unit {
        token: "kcal",
        singular: "kilocalorie",
        plural: "kilocalories",
        exact_case: false,
    },
    Unit {
        token: "kmh",
        singular: "kilometer per hour",
        plural: "kilometers per hour",
        exact_case: false,
    },
    Unit {
        token: "kph",
        singular: "kilometer per hour",
        plural: "kilometers per hour",
        exact_case: false,
    },
    Unit {
        token: "mph",
        singular: "mile per hour",
        plural: "miles per hour",
        exact_case: false,
    },
    Unit {
        token: "m/s",
        singular: "meter per second",
        plural: "meters per second",
        exact_case: false,
    },
    Unit {
        token: "kwh",
        singular: "kilowatt hour",
        plural: "kilowatt hours",
        exact_case: false,
    },
    Unit {
        token: "dpi",
        singular: "dot per inch",
        plural: "dots per inch",
        exact_case: false,
    },
    Unit {
        token: "rpm",
        singular: "revolution per minute",
        plural: "revolutions per minute",
        exact_case: false,
    },
    Unit {
        token: "psi",
        singular: "pound per square inch",
        plural: "pounds per square inch",
        exact_case: false,
    },
    Unit {
        token: "fps",
        singular: "frame per second",
        plural: "frames per second",
        exact_case: false,
    },
    Unit {
        token: "khz",
        singular: "kilohertz",
        plural: "kilohertz",
        exact_case: false,
    },
    Unit {
        token: "mhz",
        singular: "megahertz",
        plural: "megahertz",
        exact_case: false,
    },
    Unit {
        token: "ghz",
        singular: "gigahertz",
        plural: "gigahertz",
        exact_case: false,
    },
    Unit {
        token: "mbar",
        singular: "millibar",
        plural: "millibars",
        exact_case: false,
    },
    Unit {
        token: "kg",
        singular: "kilogram",
        plural: "kilograms",
        exact_case: false,
    },
    Unit {
        token: "mg",
        singular: "milligram",
        plural: "milligrams",
        exact_case: false,
    },
    Unit {
        token: "cm",
        singular: "centimeter",
        plural: "centimeters",
        exact_case: false,
    },
    Unit {
        token: "mm",
        singular: "millimeter",
        plural: "millimeters",
        exact_case: false,
    },
    Unit {
        token: "km",
        singular: "kilometer",
        plural: "kilometers",
        exact_case: false,
    },
    Unit {
        token: "ml",
        singular: "milliliter",
        plural: "milliliters",
        exact_case: false,
    },
    Unit {
        token: "lbs",
        singular: "pound",
        plural: "pounds",
        exact_case: false,
    },
    Unit {
        token: "lb",
        singular: "pound",
        plural: "pounds",
        exact_case: false,
    },
    Unit {
        token: "oz",
        singular: "ounce",
        plural: "ounces",
        exact_case: false,
    },
    Unit {
        token: "gal",
        singular: "gallon",
        plural: "gallons",
        exact_case: false,
    },
    Unit {
        token: "min",
        singular: "minute",
        plural: "minutes",
        exact_case: false,
    },
    Unit {
        token: "hrs",
        singular: "hour",
        plural: "hours",
        exact_case: false,
    },
    Unit {
        token: "hr",
        singular: "hour",
        plural: "hours",
        exact_case: false,
    },
    Unit {
        token: "h",
        singular: "hour",
        plural: "hours",
        exact_case: false,
    },
    Unit {
        token: "sec",
        singular: "second",
        plural: "seconds",
        exact_case: false,
    },
    Unit {
        token: "kb",
        singular: "kilobyte",
        plural: "kilobytes",
        exact_case: false,
    },
    Unit {
        token: "mb",
        singular: "megabyte",
        plural: "megabytes",
        exact_case: false,
    },
    Unit {
        token: "gb",
        singular: "gigabyte",
        plural: "gigabytes",
        exact_case: false,
    },
    Unit {
        token: "tb",
        singular: "terabyte",
        plural: "terabytes",
        exact_case: false,
    },
    Unit {
        token: "ft",
        singular: "foot",
        plural: "feet",
        exact_case: false,
    },
    Unit {
        token: "yd",
        singular: "yard",
        plural: "yards",
        exact_case: false,
    },
    Unit {
        token: "mi",
        singular: "mile",
        plural: "miles",
        exact_case: false,
    },
    Unit {
        token: "in",
        singular: "inch",
        plural: "inches",
        exact_case: false,
    },
    Unit {
        token: "m",
        singular: "meter",
        plural: "meters",
        exact_case: true,
    },
    Unit {
        token: "g",
        singular: "gram",
        plural: "grams",
        exact_case: true,
    },
    Unit {
        token: "l",
        singular: "liter",
        plural: "liters",
        exact_case: true,
    },
    Unit {
        token: "s",
        singular: "second",
        plural: "seconds",
        exact_case: true,
    },
];

/// A magnitude after a currency amount: `$1m` is one million dollars.
const MAGNITUDES: [(&str, &str); 4] = [
    ("bn", "billion"),
    ("k", "thousand"),
    ("m", "million"),
    ("b", "billion"),
];

/// How text is expanded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NormalizeOptions {
    /// Say the part of the day after a clock time: "seven forty five in the
    /// evening". A written `am` or `pm` decides it; otherwise the hour does.
    pub time_of_day: bool,
}

impl Default for NormalizeOptions {
    fn default() -> Self {
        Self { time_of_day: true }
    }
}

/// Expand numbers, currency, times, percentages, and common abbreviations,
/// with the default [`NormalizeOptions`].
#[must_use]
pub fn normalize(text: &str) -> String {
    normalize_with(text, NormalizeOptions::default())
}

/// Expand with explicit [`NormalizeOptions`].
#[must_use]
pub fn normalize_with(text: &str, options: NormalizeOptions) -> String {
    let expanded = expand_abbreviations(text);
    // Nothing to expand: no digit to read and no currency to speak. Collecting the text
    // into `Vec<char>` costs four bytes a character, and most sentences reach here.
    if !expanded
        .bytes()
        .any(|byte| byte.is_ascii_digit() || byte == b'$')
    {
        // Nothing to expand: the borrowed case copies once here, and only once.
        return expanded.into_owned();
    }
    let chars: Vec<char> = expanded.chars().collect();
    let mut out = String::with_capacity(expanded.len() * 2);
    let mut index = 0;

    while index < chars.len() {
        let ch = chars[index];

        if ch == '$' {
            if let Some(scanned) = scan_number(&chars, index + 1) {
                // `$1m` is one million dollars, and `$2.5b` is two point five billion.
                if let Some((magnitude, after)) = scan_magnitude(&chars, scanned.end) {
                    write_number(&scanned, &mut out);
                    out.push(' ');
                    out.push_str(magnitude);
                    out.push_str(" dollars");
                    index = after;
                    continue;
                }
                write_currency(&scanned, &mut out);
                index = scanned.end;
                continue;
            }
        }

        if ch.is_ascii_digit() {
            if let Some((hour, minute, second, next)) = scan_time(&chars, index) {
                // A written marker decides the period and is consumed, so
                // "7:45 pm" does not leave "p m" behind.
                let (hour, next) = match scan_marker(&chars, next) {
                    Some((pm, after)) => {
                        let hour = if pm { hour % 12 + 12 } else { hour % 12 };
                        (hour, after)
                    }
                    None => (hour, next),
                };
                write_clock(hour, minute, second, options, &mut out);
                index = next;
                continue;
            }
            if let Some(scanned) = scan_number(&chars, index) {
                if let Some((_, after)) = scan_ordinal_suffix(&chars, scanned.end) {
                    write_ordinal(scanned.whole, &mut out);
                    index = after;
                    continue;
                }
                if chars.get(scanned.end) == Some(&'%') {
                    write_number(&scanned, &mut out);
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
                        write_clock(hour, 0, 0, options, &mut out);
                        index = after;
                        continue;
                    }
                }
                // A unit written straight after the number: `5kg`, `190cm`, `60mph`.
                if let Some((unit, after)) = scan_unit(&chars, scanned.end) {
                    let singular = scanned.whole == 1 && scanned.fraction.is_empty();
                    write_number(&scanned, &mut out);
                    out.push(' ');
                    out.push_str(if singular { unit.singular } else { unit.plural });
                    index = scanned.end + after;
                    continue;
                }
                write_number(&scanned, &mut out);
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
///
/// The result borrows the input when nothing matched. Almost every sentence reaches here
/// with a full stop and no abbreviation, and copying it before looking was the cost.
fn expand_abbreviations(text: &str) -> Cow<'_, str> {
    // Every abbreviation ends in a full stop, so text without one has nothing to expand.
    if !text.contains('.') {
        return Cow::Borrowed(text);
    }
    let mut out: Option<String> = None;
    let mut index = 0;

    while index < text.len() {
        if let Some((short, long)) = abbreviation_at(text, index) {
            // The first match starts the copy: everything before it is unchanged.
            let buffer = out.get_or_insert_with(|| {
                let mut buffer = String::with_capacity(text.len());
                buffer.push_str(&text[..index]);
                buffer
            });
            let source = &text[index..index + short.len()];
            if source.chars().next().is_some_and(char::is_uppercase) {
                let mut chars = long.chars();
                if let Some(first) = chars.next() {
                    buffer.extend(first.to_uppercase());
                    buffer.push_str(chars.as_str());
                }
            } else {
                buffer.push_str(long);
            }
            index += short.len();
            continue;
        }

        let ch = text[index..]
            .chars()
            .next()
            .expect("index is on a boundary");
        if let Some(buffer) = out.as_mut() {
            buffer.push(ch);
        }
        index += ch.len_utf8();
    }

    match out {
        Some(buffer) => Cow::Owned(buffer),
        None => Cow::Borrowed(text),
    }
}

/// The abbreviation that starts at `index`, if one does.
///
/// The comparison is against the written bytes, ignoring ASCII case, so no lowercase copy
/// of the sentence is made. The first byte rules out most positions before any slicing:
/// every token in the table starts with one of the letters below.
fn abbreviation_at(text: &str, index: usize) -> Option<(&'static str, &'static str)> {
    let first = *text.as_bytes().get(index)?;
    if !starts_abbreviation(first) {
        return None;
    }
    // Only at a word start, so "address." does not match "dr.".
    if index > 0
        && text[..index]
            .chars()
            .next_back()
            .is_some_and(|previous| previous.is_alphanumeric())
    {
        return None;
    }
    ABBREVIATIONS
        .iter()
        .copied()
        .find(|(short, _)| written_starts_with(text.as_bytes(), index, short))
}

/// Whether a token in [`ABBREVIATIONS`] could start with this byte.
///
/// The test below it keeps this in step with the table.
fn starts_abbreviation(byte: u8) -> bool {
    matches!(
        byte.to_ascii_lowercase(),
        b'a' | b'd' | b'e' | b'i' | b'm' | b's' | b'v'
    )
}

/// Whether `bytes[index..]` begins with the ASCII `token`, ignoring ASCII case.
fn written_starts_with(bytes: &[u8], index: usize, token: &str) -> bool {
    let token = token.as_bytes();
    bytes.len() >= index + token.len()
        && bytes[index..index + token.len()]
            .iter()
            .zip(token)
            .all(|(got, want)| got.eq_ignore_ascii_case(want))
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

/// Whether the comma at `index` is a thousands separator.
///
/// It has to be followed by exactly three digits and then something that is not a digit.
/// Taking any comma turned `In 2026, hello` into `In two thousand twenty six hello`: the
/// comma was eaten and the year changed reading from `twenty twenty six`.
fn is_thousands_separator(chars: &[char], index: usize) -> bool {
    let digits = chars
        .get(index + 1..index + 4)
        .is_some_and(|run| run.iter().all(char::is_ascii_digit));
    let ends = !chars.get(index + 4).is_some_and(char::is_ascii_digit);
    digits && ends
}

/// Whether `slice` spells `token`, ignoring ASCII case when asked.
///
/// The tables are ASCII and a candidate is at most four characters, so a comparison walks
/// the slice. Building a `String` per candidate cost more than the scan it was there to do.
fn spells(slice: &[char], token: &str, ignore_case: bool) -> bool {
    let mut wanted = token.chars();
    for got in slice {
        match wanted.next() {
            Some(want) => {
                let same = if ignore_case {
                    got.eq_ignore_ascii_case(&want)
                } else {
                    *got == want
                };
                if !same {
                    return false;
                }
            }
            None => return false,
        }
    }
    wanted.next().is_none()
}

/// The same, at an offset in a character slice.
fn spells_at(chars: &[char], start: usize, token: &str, ignore_case: bool) -> bool {
    chars
        .get(start..start + token.chars().count())
        .is_some_and(|slice| spells(slice, token, ignore_case))
}

/// A temperature, written with the degree sign.
const DEGREE_CELSIUS: Unit = Unit {
    token: "°c",
    singular: "degree Celsius",
    plural: "degrees Celsius",
    exact_case: false,
};

/// The same in Fahrenheit.
const DEGREE_FAHRENHEIT: Unit = Unit {
    token: "°f",
    singular: "degree Fahrenheit",
    plural: "degrees Fahrenheit",
    exact_case: false,
};

/// The unit written straight after a number, and how many characters it takes.
///
/// Longest match first, so `km/h` wins over `km` and `hrs` over `hr`. A unit has to end
/// the token: `5kgs` is not five kilograms followed by something.
fn scan_unit(chars: &[char], start: usize) -> Option<(&'static Unit, usize)> {
    if let Some(found) = scan_unit_attached(chars, start) {
        return Some(found);
    }
    // One space is allowed before an unambiguous token: `190 cm` is a length, while
    // `5 in the morning` and `2 m of rope` are the same shape and are not. A token of
    // two characters or more that is not also a word carries no such doubt.
    if chars.get(start) == Some(&' ') {
        if let Some((unit, length)) = scan_unit_attached(chars, start + 1) {
            if unit.token.len() >= 2 && !matches!(unit.token, "in" | "gal") {
                return Some((unit, 1 + length));
            }
        }
    }
    None
}

/// The same, with no space before it.
fn scan_unit_attached(chars: &[char], start: usize) -> Option<(&'static Unit, usize)> {
    if chars.get(start) == Some(&'°') {
        return match chars.get(start + 1).map(char::to_ascii_lowercase) {
            Some('c') => Some((&DEGREE_CELSIUS, 2)),
            Some('f') => Some((&DEGREE_FAHRENHEIT, 2)),
            _ => None,
        };
    }
    for length in (1..=4).rev() {
        let Some(slice) = chars.get(start..start + length) else {
            continue;
        };
        if !slice
            .iter()
            .all(|ch| ch.is_ascii_alphabetic() || *ch == '/')
        {
            continue;
        }
        if chars
            .get(start + length)
            .is_some_and(|ch| ch.is_alphanumeric())
        {
            continue;
        }
        let found = UNITS
            .iter()
            .find(|unit| unit.token.len() == length && spells(slice, unit.token, !unit.exact_case));
        if let Some(unit) = found {
            return Some((unit, length));
        }
    }
    None
}

/// A magnitude written after a currency amount: the `m` in `$1m`.
fn scan_magnitude(chars: &[char], start: usize) -> Option<(&'static str, usize)> {
    for (token, word) in MAGNITUDES {
        let length = token.len();
        if spells_at(chars, start, token, true)
            && !chars
                .get(start + length)
                .is_some_and(|ch| ch.is_alphanumeric())
        {
            return Some((word, start + length));
        }
    }
    None
}

/// Read digits, optional thousands separators, and an optional decimal part.
fn scan_number(chars: &[char], start: usize) -> Option<Scanned> {
    let mut index = start;
    let mut whole = 0u64;
    let mut digits = 0;
    let mut separated = false;
    // Digits since the last separator, or since the start. A comma is a thousands
    // separator only when it groups three digits, and only when the group before it is a
    // plausible head: `2026,100` is a year and a new clause, not two million.
    let mut group = 0usize;

    while let Some(&ch) = chars.get(index) {
        if ch.is_ascii_digit() {
            whole = whole
                .checked_mul(10)?
                .checked_add(u64::from(ch as u8 - b'0'))?;
            digits += 1;
            group += 1;
            index += 1;
        } else if ch == ','
            && digits > 0
            && is_thousands_separator(chars, index)
            && (1..=3).contains(&group)
            && (!separated || group == 3)
        {
            separated = true;
            group = 0;
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

/// Read `H:MM`, `HH:MM`, or `H:MM:SS`, with hours, minutes, seconds, and the
/// index after.
fn scan_time(chars: &[char], start: usize) -> Option<(u64, u64, u64, usize)> {
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

    // Seconds are optional, and consumed when present, so "19:45:30" does not
    // leave a stray colon and an unexpanded number behind.
    let mut second = 0;
    let mut end = after;
    if chars.get(end) == Some(&':') {
        if let (Some(high), Some(low)) = (chars.get(end + 1), chars.get(end + 2)) {
            if let (Some(high), Some(low)) = (high.to_digit(10), low.to_digit(10)) {
                let value = high * 10 + low;
                if value < 60 {
                    second = u64::from(value);
                    end += 3;
                }
            }
        }
    }

    Some((hour, minute, second, end))
}

/// Read an ordinal suffix and return it with the index after.
fn scan_ordinal_suffix(chars: &[char], start: usize) -> Option<(&'static str, usize)> {
    // The suffix has to end the word, with one exception: an adverb keeps it, because
    // `2ndly` is `secondly` and a voice should say that. Without the boundary, `1stand`
    // reads as `first` + `and` and the reader hears `firstand`.
    let lower = |offset: usize| chars.get(start + offset).map(|ch| ch.to_ascii_lowercase());
    let follows_alnum = chars.get(start + 2).is_some_and(|ch| ch.is_alphanumeric());
    let adverb = lower(2) == Some('l')
        && lower(3) == Some('y')
        && !chars.get(start + 4).is_some_and(|ch| ch.is_alphanumeric());
    if follows_alnum && !adverb {
        return None;
    }
    let suffix = ["st", "nd", "rd", "th"]
        .into_iter()
        .find(|suffix| spells_at(chars, start, suffix, true))?;
    Some((suffix, start + 2))
}

/// Words for a scanned number, choosing the year reading for four-digit years.
fn write_number(scanned: &Scanned, out: &mut String) {
    if !scanned.fraction.is_empty() {
        write_cardinal(scanned.whole, out);
        out.push_str(" point");
        for digit in &scanned.fraction {
            out.push(' ');
            out.push_str(ONES[*digit as usize]);
        }
        return;
    }

    if scanned.digits == 4 && !scanned.separated && (1100..=2099).contains(&scanned.whole) {
        let high = scanned.whole / 100;
        let low = scanned.whole % 100;
        write_cardinal(high, out);
        if low == 0 {
            out.push_str(" hundred");
        } else {
            out.push(' ');
            write_cardinal(low, out);
        }
        return;
    }

    write_cardinal(scanned.whole, out);
}

/// Words for a whole number below one trillion.
#[must_use]
pub fn cardinal(value: u64) -> String {
    let mut out = String::new();
    write_cardinal(value, &mut out);
    out
}

/// Push the words for a whole number below one trillion, one space between words.
///
/// Every word is a `&'static str` pushed into the caller's buffer, and nothing is written
/// before `out`'s existing content: the previous version built a `Vec<String>` of `format!`
/// output at every level of the recursion, so one number cost a dozen allocations.
fn write_cardinal(value: u64, out: &mut String) {
    let start = out.len();
    write_cardinal_words(value, out, start);
}

/// The same, knowing where this number's first word begins in `out`.
fn write_cardinal_words(value: u64, out: &mut String, start: usize) {
    if value == 0 {
        push_word(out, start, ONES[0]);
        return;
    }

    let mut rest = value;
    for (divisor, name) in [
        (1_000_000_000u64, "billion"),
        (1_000_000, "million"),
        (1_000, "thousand"),
    ] {
        if rest >= divisor {
            write_cardinal_words(rest / divisor, out, start);
            out.push(' ');
            out.push_str(name);
            rest %= divisor;
        }
    }

    if rest > 0 {
        write_below_thousand(rest, out, start);
    }
}

/// One word, separated from the words already written for this number.
fn push_word(out: &mut String, start: usize, word: &str) {
    if out.len() > start {
        out.push(' ');
    }
    out.push_str(word);
}

fn write_below_thousand(value: u64, out: &mut String, start: usize) {
    let hundreds = value / 100;
    let rest = value % 100;

    if hundreds > 0 {
        push_word(out, start, ONES[hundreds as usize]);
        out.push_str(" hundred");
    }
    if rest > 0 {
        if rest < 20 {
            push_word(out, start, ONES[rest as usize]);
        } else {
            let tens = (rest / 10) as usize;
            let ones = (rest % 10) as usize;
            push_word(out, start, TENS[tens]);
            if ones > 0 {
                push_word(out, start, ONES[ones]);
            }
        }
    }
}

/// The ordinal form of a value: `3` with `rd` becomes `third`.
#[must_use]
pub fn ordinal(value: u64, _suffix: &str) -> String {
    let mut out = String::new();
    write_ordinal(value, &mut out);
    out
}

/// The same, appended to `out`.
fn write_ordinal(value: u64, out: &mut String) {
    let begin = out.len();
    write_cardinal(value, out);
    let last_start = out[begin..]
        .rfind(' ')
        .map_or(begin, |space| begin + space + 1);
    let last = &out[last_start..];

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
    let stem_ends_in_y = last.ends_with('y');

    match irregular {
        Some(word) => {
            out.truncate(last_start);
            out.push_str(word);
        }
        None if stem_ends_in_y => {
            out.truncate(out.len() - 1);
            out.push_str("ieth");
        }
        None => out.push_str("th"),
    }
}

/// Dollars and cents.
///
/// A fraction longer than two digits is not a number of cents: `$0.005` is half a cent, and
/// reading the first two digits said "zero dollars", which is the wrong amount by the whole
/// point of writing it. Those are read as the number written instead: "zero point zero zero
/// five dollars".
fn write_currency(scanned: &Scanned, out: &mut String) {
    let dollars = scanned.whole;
    let fraction = &scanned.fraction;
    if fraction.len() > 2 {
        write_number(scanned, out);
        out.push_str(" dollars");
        return;
    }
    let cents = fraction.first().copied().unwrap_or(0) * 10 + fraction.get(1).copied().unwrap_or(0);
    match (dollars, cents) {
        (0, 0) => out.push_str("zero dollars"),
        (0, cents) => {
            write_cardinal(cents, out);
            out.push_str(" cents");
        }
        (1, 0) => out.push_str("one dollar"),
        (dollars, 0) => {
            write_cardinal(dollars, out);
            out.push_str(" dollars");
        }
        (dollars, cents) => {
            write_cardinal(dollars, out);
            out.push_str(if dollars == 1 { " dollar" } else { " dollars" });
            out.push_str(" and ");
            write_cardinal(cents, out);
            out.push_str(if cents == 1 { " cent" } else { " cents" });
        }
    }
}

/// The clock reading of an hour and minute pair.
fn write_clock(hour: u64, minute: u64, second: u64, options: NormalizeOptions, out: &mut String) {
    let twelve = if hour % 12 == 0 { 12 } else { hour % 12 };
    match minute {
        0 => {
            write_cardinal(twelve, out);
            out.push_str(" o'clock");
        }
        minute if minute < 10 => {
            write_cardinal(twelve, out);
            out.push_str(" oh ");
            write_cardinal(minute, out);
        }
        minute => {
            write_cardinal(twelve, out);
            out.push(' ');
            write_cardinal(minute, out);
        }
    }
    if second > 0 {
        out.push_str(" and ");
        write_cardinal(second, out);
        out.push_str(if second == 1 { " second" } else { " seconds" });
    }
    if options.time_of_day {
        out.push_str(period(hour));
    }
}

/// The part of the day, from the hour on a 24-hour clock.
///
/// Night is the late evening and the small hours, so 22:00 and 02:00 both read as night
/// and the two are not split across a boundary a listener would not hear.
fn period(hour: u64) -> &'static str {
    match hour {
        6..=11 => " in the morning",
        12..=17 => " in the afternoon",
        18..=21 => " in the evening",
        _ => " at night",
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
    if !chars
        .get(cursor)
        .is_some_and(|ch| ch.eq_ignore_ascii_case(&'m'))
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
    fn a_thousands_separator_needs_the_right_digit_count_before_it() {
        // `2026,100` was read as 2,026,100: a four-digit year followed by a comma is a
        // year and a new clause, not a group of three.
        assert_eq!(normalize("2026,100"), "twenty twenty six,one hundred");
        // A comma inside the fractional part is punctuation too.
        assert_eq!(normalize("1.5,000"), "one point five,zero");
        // The shape that is a separator still is one.
        assert_eq!(normalize("1,000"), "one thousand");
        assert_eq!(
            normalize("12,345,678"),
            "twelve million three hundred forty five thousand six hundred seventy eight"
        );
    }

    #[test]
    fn a_comma_after_a_year_is_not_a_thousands_separator() {
        // The comma was eaten and the year changed reading: `two thousand twenty six`
        // instead of `twenty twenty six`.
        assert_eq!(normalize("In 2026, hello"), "In twenty twenty six, hello");
        assert_eq!(normalize("2026"), "twenty twenty six");
        // A real separator still works, in both forms.
        assert_eq!(normalize("1,000 items"), "one thousand items");
        assert_eq!(
            normalize("12,345,678"),
            "twelve million three hundred forty five thousand six hundred seventy eight"
        );
        // A comma between digits that is not a group of three is left alone.
        assert!(!normalize("1,2 things").contains("twelve"));
    }

    #[test]
    fn an_ordinal_suffix_must_end_its_word() {
        // `1stand` used to read as `first` + `and`.
        assert_eq!(normalize("1stand"), "onestand");
        assert_eq!(normalize("the 1st and the 2nd"), "the first and the second");
        // An adverb keeps its ordinal, because `secondly` is the word.
        assert_eq!(normalize("2ndly"), "secondly");
    }

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
    fn a_unit_attached_to_a_number_is_spoken() {
        // A digit followed by letters used to run together: `5kg` came out as one
        // unpronounceable token, and the voice had to guess at it.
        assert_eq!(normalize("5kg"), "five kilograms");
        assert_eq!(normalize("1kg"), "one kilogram");
        assert_eq!(normalize("190cm"), "one hundred ninety centimeters");
        assert_eq!(normalize("1.5kg"), "one point five kilograms");
        assert_eq!(normalize("500mb"), "five hundred megabytes");
        assert_eq!(normalize("2h"), "two hours");
        assert_eq!(normalize("1ft"), "one foot");
        assert_eq!(normalize("60mph"), "sixty miles per hour");
        assert_eq!(normalize("20°C"), "twenty degrees Celsius");
        // A unit inside a sentence keeps its neighbours.
        assert_eq!(normalize("the box is 5kg"), "the box is five kilograms");
    }

    #[test]
    fn a_space_before_an_unambiguous_unit_is_accepted() {
        assert_eq!(normalize("190 cm"), "one hundred ninety centimeters");
        assert_eq!(normalize("5 kg"), "five kilograms");
        assert_eq!(normalize("20 °C"), "twenty degrees Celsius");
        assert_eq!(normalize("5 min"), "five minutes");
        // Not accepted: the same shape, but the token is a word or a single letter.
        assert_eq!(normalize("5 in the morning"), "five in the morning");
        assert_eq!(normalize("2 m of rope"), "two m of rope");
        assert_eq!(normalize("2 gal"), "two gal");
    }

    #[test]
    fn a_currency_magnitude_suffix_is_spoken() {
        assert_eq!(normalize("$1m"), "one million dollars");
        assert_eq!(normalize("$5k"), "five thousand dollars");
        assert_eq!(normalize("$2.5b"), "two point five billion dollars");
        // Without a suffix nothing changes.
        assert_eq!(normalize("$1,000"), "one thousand dollars");
    }

    #[test]
    fn currency() {
        assert_eq!(normalize("$3.99"), "three dollars and ninety nine cents");
        assert_eq!(normalize("$1"), "one dollar");
        assert_eq!(normalize("$0.50"), "fifty cents");
        assert_eq!(normalize("$1.01"), "one dollar and one cent");
    }

    #[test]
    fn a_currency_amount_with_sub_cent_digits_is_read_as_written() {
        // Two digits of the fraction were read as cents and the rest were dropped, so
        // `$0.005` was spoken as "zero dollars": an amount that is not zero, said as zero.
        assert_eq!(normalize("$0.005"), "zero point zero zero five dollars");
        assert_eq!(normalize("$1.234"), "one point two three four dollars");
        // Two digits or fewer still read as dollars and cents.
        assert_eq!(normalize("$0.05"), "five cents");
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
        assert_eq!(normalize("00:30"), "twelve thirty at night");
        assert_eq!(normalize("17:59"), "five fifty nine in the afternoon");
        assert_eq!(normalize("18:00"), "six o'clock in the evening");
        assert_eq!(normalize("21:59"), "nine fifty nine in the evening");
        assert_eq!(normalize("22:00"), "ten o'clock at night");
        assert_eq!(normalize("23:59"), "eleven fifty nine at night");
        assert_eq!(normalize("02:00"), "two o'clock at night");
        assert_eq!(normalize("05:59"), "five fifty nine at night");
        assert_eq!(normalize("06:00"), "six o'clock in the morning");
    }

    #[test]
    fn without_the_option_the_hour_stands_alone() {
        let options = NormalizeOptions { time_of_day: false };
        assert_eq!(normalize_with("19:45", options), "seven forty five");
        assert_eq!(normalize_with("07:45", options), "seven forty five");
    }

    #[test]
    fn a_written_marker_wins_and_is_consumed() {
        assert_eq!(normalize("7:45 pm"), "seven forty five in the evening");
        assert_eq!(normalize("7:45pm"), "seven forty five in the evening");
        assert_eq!(normalize("7:45 a.m."), "seven forty five in the morning");
        // The marker sets the hour; the table still decides the word, and half past
        // midnight is night.
        assert_eq!(normalize("12:30 am"), "twelve thirty at night");
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
            "meet at twelve o'clock at night"
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
        assert_eq!(normalize("00:15"), "twelve fifteen at night");
        assert_eq!(normalize("9:41"), "nine forty one in the morning");
    }

    #[test]
    fn seconds_are_consumed_and_spoken() {
        assert_eq!(
            normalize("19:45:30"),
            "seven forty five and thirty seconds in the evening"
        );
        assert_eq!(normalize("19:45:00"), "seven forty five in the evening");
        assert_eq!(
            normalize("19:45:01"),
            "seven forty five and one second in the evening"
        );
        // Not a valid second, so the colon stays and the number is spoken as one.
        assert_eq!(
            normalize("19:45:99"),
            "seven forty five in the evening:ninety nine"
        );
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
    fn every_abbreviation_passes_the_first_byte_gate() {
        // The gate is a `matches!` list rather than a scan of the table, so it has to be
        // kept in step with it.
        for (short, _) in ABBREVIATIONS {
            let first = short.as_bytes()[0];
            assert!(starts_abbreviation(first), "{short}");
            assert!(
                starts_abbreviation(first.to_ascii_uppercase()),
                "{short} in capitals"
            );
        }
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
