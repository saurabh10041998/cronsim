//! Month and weekday names and lengths, delegated to the `time` crate.
//! Cron numbers months 1-12 and weekdays 0-7 from Sunday (7 is Sunday again).

use time::{Month, Weekday};

/// Any leap year, so February has 29 days.
const LEAP_YEAR: i32 = 2000;

fn month(value: u8) -> Month {
    Month::try_from(value).expect("cron months are 1-12")
}

fn weekday(value: u8) -> Weekday {
    Weekday::Sunday.nth_next(value)
}

pub fn month_name(value: u8) -> String {
    month(value).to_string()
}

pub fn weekday_name(value: u8) -> String {
    weekday(value).to_string()
}

/// The longest the month can be.
pub fn max_days(month_value: u8) -> u8 {
    month(month_value).length(LEAP_YEAR)
}

/// Cron accepts three-letter names, case-insensitive: `jan`, `MON`.
pub fn parse_month(token: &str) -> Option<u8> {
    (1..=12).find(|&m| is_abbreviation(token, &month_name(m)))
}

pub fn parse_weekday(token: &str) -> Option<u8> {
    (0..7).find(|&d| is_abbreviation(token, &weekday_name(d)))
}

fn is_abbreviation(token: &str, name: &str) -> bool {
    token.len() == 3 && name[..3].eq_ignore_ascii_case(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_lengths() {
        assert_eq!(month_name(1), "January");
        assert_eq!(month_name(12), "December");
        assert_eq!(weekday_name(0), "Sunday");
        assert_eq!(weekday_name(6), "Saturday");
        assert_eq!(weekday_name(7), "Sunday");
        assert_eq!(max_days(2), 29);
        assert_eq!(max_days(4), 30);
    }

    #[test]
    fn parses_three_letter_names_only() {
        assert_eq!(parse_month("jan"), Some(1));
        assert_eq!(parse_month("DEC"), Some(12));
        assert_eq!(parse_weekday("sun"), Some(0));
        assert_eq!(parse_weekday("Sat"), Some(6));
        for bad in ["January", "ja", "", "xyz"] {
            assert_eq!(parse_month(bad), None, "{bad:?}");
        }
        assert_eq!(parse_weekday("Monday"), None);
    }
}
