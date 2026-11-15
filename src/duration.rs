//! Parsing of human-readable durations such as `1 day 12 hours` or `90m`.

use anyhow::{Result, bail};

const UNITS: &[(&[&str], u64)] = &[
    (&["y", "year", "years"], 365 * 86_400),
    (&["month", "months"], 30 * 86_400),
    (&["w", "week", "weeks"], 7 * 86_400),
    (&["d", "day", "days"], 86_400),
    (&["h", "hour", "hours"], 3_600),
    (&["m", "min", "mins", "minute", "minutes"], 60),
    (&["s", "sec", "secs", "second", "seconds"], 1),
];

/// Parse a duration into seconds.
///
/// A duration is a sequence of `<number> <unit>` pairs, optionally separated
/// by whitespace or commas, e.g. `5 minutes`, `1h30m` or `1 year, 2 months`.
/// Each unit may be used once. A year has 365 days and a month 30 days.
pub fn parse_duration(input: &str) -> Result<u64> {
    let lower = input.to_ascii_lowercase();
    let mut rest = lower.trim();
    let mut total: u64 = 0;
    let mut used = [false; UNITS.len()];
    if rest.is_empty() {
        bail!("the duration is empty");
    }
    while !rest.is_empty() {
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits == 0 {
            bail!("invalid element in duration \"{input}\": \"{rest}\"");
        }
        let Ok(amount) = rest[..digits].parse::<u64>() else {
            bail!("the number in duration \"{input}\" is too large");
        };
        rest = rest[digits..].trim_start();
        let letters = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_alphabetic()).len();
        let unit = &rest[..letters];
        let Some(index) = UNITS.iter().position(|(names, _)| names.contains(&unit)) else {
            if unit.is_empty() {
                bail!("missing unit after \"{amount}\" in duration \"{input}\" (e.g. \"{amount} minutes\")");
            }
            bail!("unknown unit \"{unit}\" in duration \"{input}\"");
        };
        if std::mem::replace(&mut used[index], true) {
            bail!("unit \"{unit}\" is used more than once in duration \"{input}\"");
        }
        let Some(sum) = amount.checked_mul(UNITS[index].1).and_then(|seconds| total.checked_add(seconds)) else {
            bail!("duration \"{input}\" is too long");
        };
        total = sum;
        rest = rest[letters..].trim_start_matches(|c: char| c.is_whitespace() || c == ',');
    }
    Ok(total)
}

/// Format seconds the way [`parse_duration`] reads them, e.g. `1d 2h 3m 4s`.
pub fn format_duration(mut seconds: u64) -> String {
    if seconds == 0 {
        return "0s".to_string();
    }
    let mut parts = Vec::new();
    for (unit, size) in [("y", 365 * 86_400), ("d", 86_400), ("h", 3_600), ("m", 60), ("s", 1)] {
        if seconds >= size {
            parts.push(format!("{}{unit}", seconds / size));
            seconds %= size;
        }
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_units() {
        for (input, expected) in [
            ("0s", 0),
            ("1 minute", 60),
            ("5 minutes", 300),
            ("5mins", 300),
            ("90m", 5400),
            ("1 day", 86_400),
            ("2d", 172_800),
            ("3 hours", 10_800),
            ("1h", 3_600),
            ("1 week", 604_800),
            ("1 month", 2_592_000),
            ("2 years", 63_072_000),
            ("1y", 31_536_000),
            ("45 secs", 45),
            ("7 Seconds", 7),
            ("  10   s  ", 10),
        ] {
            assert_eq!(parse_duration(input).unwrap(), expected, "{input}");
        }
    }

    #[test]
    fn parses_combinations() {
        assert_eq!(parse_duration("1h30m").unwrap(), 5400);
        assert_eq!(parse_duration("1 day 12 hours").unwrap(), 129_600);
        assert_eq!(parse_duration("1 year, 2 months, 3 days").unwrap(), 31_536_000 + 5_184_000 + 259_200);
        assert_eq!(parse_duration("30s 2m").unwrap(), 150);
    }

    #[test]
    fn rejects_garbage() {
        for bad in
            ["", "  ", "5", "minutes", "5 parsecs", "-5 minutes", "5 minutes ago", "1.5h", "5m 5min", "99999999999999999999s", "18446744073709551615y", "5 m 3"]
        {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn formats() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(59), "59s");
        assert_eq!(format_duration(93_784), "1d 2h 3m 4s");
        assert_eq!(format_duration(31_536_000 + 60), "1y 1m");
        for seconds in [1, 60, 3_600, 86_400, 123_456_789] {
            assert_eq!(parse_duration(&format_duration(seconds)).unwrap(), seconds);
        }
    }
}
