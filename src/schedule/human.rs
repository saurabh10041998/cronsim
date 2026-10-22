use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use super::calendar;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Minute,
    Hour,
}

impl Unit {
    fn cycle(self) -> u32 {
        match self {
            Self::Minute => 60,
            Self::Hour => 24,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Minute => "minute",
            Self::Hour => "hour",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HumanError {
    BadInterval,
    Seconds,
    UnknownUnit(String),
    Uneven {
        n: String,
        unit: Unit,
    },
    MissingTime(String),
    BadTime(String),
    TimeOutOfRange {
        part: &'static str,
        value: u32,
        max: u32,
    },
    UnknownDay(String),
    Unrecognised,
}

/// `Ok(None)` when the input is not human syntax and should be parsed as cron.
pub fn to_cron(input: &str) -> Result<Option<String>, HumanError> {
    let lower = input.to_ascii_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    match words.first() {
        Some(first) if first.starts_with(|c: char| c.is_ascii_alphabetic()) => {}
        _ => return Ok(None),
    }
    match words.as_slice() {
        ["every", rest @ ..] => every(&rest.concat()).map(Some),
        [days, "at", time] => {
            let (minute, hour) = parse_time(time)?;
            Ok(Some(format!("{minute} {hour} * * {}", parse_days(days)?)))
        }
        [days] | [days, "at"] if parse_days(days).is_ok() => {
            Err(HumanError::MissingTime(days.to_string()))
        }
        _ => Err(HumanError::Unrecognised),
    }
}

fn every(interval: &str) -> Result<String, HumanError> {
    let digits = interval.bytes().take_while(u8::is_ascii_digit).count();
    let (n, unit) = interval.split_at(digits);
    let unit = match unit {
        _ if n.is_empty() => return Err(HumanError::BadInterval),
        "m" | "min" | "mins" | "minute" | "minutes" => Unit::Minute,
        "h" | "hr" | "hrs" | "hour" | "hours" => Unit::Hour,
        "s" | "sec" | "secs" | "second" | "seconds" => return Err(HumanError::Seconds),
        "" => return Err(HumanError::BadInterval),
        other => return Err(HumanError::UnknownUnit(other.to_string())),
    };
    let value = match n.parse::<u32>() {
        Ok(0) => return Err(HumanError::BadInterval),
        Ok(v) if v < unit.cycle() && unit.cycle() % v == 0 => v,
        _ => {
            return Err(HumanError::Uneven {
                n: n.to_string(),
                unit,
            });
        }
    };
    Ok(match (unit, value) {
        (Unit::Minute, 1) => "* * * * *".to_string(),
        (Unit::Minute, v) => format!("*/{v} * * * *"),
        (Unit::Hour, 1) => "0 * * * *".to_string(),
        (Unit::Hour, v) => format!("0 */{v} * * *"),
    })
}

fn parse_time(time: &str) -> Result<(u32, u32), HumanError> {
    let bad = || HumanError::BadTime(time.to_string());
    let (hour, minute) = time.split_once(':').ok_or_else(bad)?;
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if !(1..=2).contains(&hour.len()) || minute.len() != 2 || !digits(hour) || !digits(minute) {
        return Err(bad());
    }
    let (hour, minute) = (
        hour.parse().map_err(|_| bad())?,
        minute.parse().map_err(|_| bad())?,
    );
    for (part, value, max) in [("hour", hour, 23), ("minute", minute, 59)] {
        if value > max {
            return Err(HumanError::TimeOutOfRange { part, value, max });
        }
    }
    Ok((minute, hour))
}

fn parse_days(days: &str) -> Result<String, HumanError> {
    match days {
        "daily" => return Ok("*".to_string()),
        "weekdays" => return Ok("1-5".to_string()),
        _ => {}
    }
    let numbers = days
        .split(',')
        .map(|day| calendar::parse_weekday(day).ok_or_else(|| HumanError::UnknownDay(day.into())))
        .collect::<Result<BTreeSet<u8>, _>>()?;
    Ok(numbers
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(","))
}

impl fmt::Display for HumanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadInterval => f.write_str("expected an interval like 'every 15m' or 'every 2h'"),
            Self::Seconds => {
                f.write_str("seconds aren't supported; cron's smallest unit is one minute")
            }
            Self::UnknownUnit(unit) => write!(
                f,
                "unsupported unit '{unit}'; use minutes or hours, \
                 or 'daily at …' / a day list for longer intervals"
            ),
            Self::Uneven { n, unit } if n.parse() == Ok(unit.cycle()) => {
                let hint = match unit {
                    Unit::Minute => "every 1h",
                    Unit::Hour => "daily at 00:00",
                };
                write!(
                    f,
                    "every {n} {}s is too long for */N; use '{hint}'",
                    unit.name()
                )
            }
            Self::Uneven { n, unit } => {
                let cycle = unit.cycle();
                let mut valid: Vec<String> = (1..cycle)
                    .filter(|d| cycle % d == 0)
                    .map(|d| d.to_string())
                    .collect();
                let last = valid.pop().unwrap_or_default();
                let outer = match unit {
                    Unit::Minute => "hour",
                    Unit::Hour => "day",
                };
                write!(
                    f,
                    "{n} doesn't divide {cycle}, so cron can't run it evenly \
                     (*/{n} restarts every {outer}); use {} or {last} {}s",
                    valid.join(", "),
                    unit.name()
                )
            }
            Self::MissingTime(days) => write!(f, "missing time, e.g. '{days} at 02:30'"),
            Self::BadTime(time) => write!(f, "invalid time '{time}'; expected e.g. '09:30'"),
            Self::TimeOutOfRange { part, value, max } => {
                write!(f, "{part} {value} is out of range 0-{max}")
            }
            Self::UnknownDay(day) => write!(
                f,
                "unknown day '{day}'; use sun, mon, tue, wed, thu, fri or sat"
            ),
            Self::Unrecognised => f.write_str(
                "unrecognised schedule; expected a cron expression, an @alias, \
                 or e.g. 'every 15m', 'daily at 02:30', 'mon,thu at 18:15'",
            ),
        }
    }
}

impl Error for HumanError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translations() {
        for (input, cron) in [
            ("every 15m", "*/15 * * * *"),
            ("every 15 m", "*/15 * * * *"),
            ("every 15min", "*/15 * * * *"),
            ("every 15 minutes", "*/15 * * * *"),
            ("every 30 mins", "*/30 * * * *"),
            ("every 1m", "* * * * *"),
            ("every 2h", "0 */2 * * *"),
            ("every 12 hours", "0 */12 * * *"),
            ("every 1 hr", "0 * * * *"),
            ("daily at 02:30", "30 2 * * *"),
            ("daily at 2:30", "30 2 * * *"),
            ("daily at 00:00", "0 0 * * *"),
            ("weekdays at 9:00", "0 9 * * 1-5"),
            ("mon,thu at 18:15", "15 18 * * 1,4"),
            ("thu,mon,mon at 18:15", "15 18 * * 1,4"),
            ("SUN at 7:05", "5 7 * * 0"),
            ("  Every   15M  ", "*/15 * * * *"),
            ("DAILY AT 23:59", "59 23 * * *"),
        ] {
            assert_eq!(to_cron(input), Ok(Some(cron.to_string())), "{input:?}");
        }
    }

    #[test]
    fn leaves_cron_alone() {
        for input in ["*/5 * * * *", "0 9 * * 1-5", "@daily", "", "  "] {
            assert_eq!(to_cron(input), Ok(None), "{input:?}");
        }
    }

    #[test]
    fn rejections() {
        use HumanError::*;
        let uneven = |n: &str, unit| Uneven {
            n: n.to_string(),
            unit,
        };
        let cases: &[(&str, HumanError)] = &[
            ("every 7m", uneven("7", Unit::Minute)),
            ("every 60m", uneven("60", Unit::Minute)),
            ("every 90m", uneven("90", Unit::Minute)),
            ("every 5h", uneven("5", Unit::Hour)),
            ("every 24h", uneven("24", Unit::Hour)),
            ("every 99999999999m", uneven("99999999999", Unit::Minute)),
            ("every 0m", BadInterval),
            ("every m", BadInterval),
            ("every 15", BadInterval),
            ("every", BadInterval),
            ("every 30s", Seconds),
            ("every 2d", UnknownUnit("d".into())),
            ("every 1w", UnknownUnit("w".into())),
            (
                "daily at 25:00",
                TimeOutOfRange {
                    part: "hour",
                    value: 25,
                    max: 23,
                },
            ),
            (
                "daily at 9:60",
                TimeOutOfRange {
                    part: "minute",
                    value: 60,
                    max: 59,
                },
            ),
            ("daily at 9", BadTime("9".into())),
            ("daily at 9:5", BadTime("9:5".into())),
            ("daily at 123:00", BadTime("123:00".into())),
            ("daily", MissingTime("daily".into())),
            ("weekdays at", MissingTime("weekdays".into())),
            ("mon,thu", MissingTime("mon,thu".into())),
            ("funday at 9:00", UnknownDay("funday".into())),
            ("mon,,thu at 9:00", UnknownDay("".into())),
            ("evry 15m", Unrecognised),
            ("hello", Unrecognised),
        ];
        for (input, expected) in cases {
            assert_eq!(to_cron(input).as_ref(), Err(expected), "{input:?}");
        }
    }

    #[test]
    fn messages() {
        let message = |input| to_cron(input).unwrap_err().to_string();
        assert_eq!(
            message("every 7m"),
            "7 doesn't divide 60, so cron can't run it evenly (*/7 restarts every hour); \
             use 1, 2, 3, 4, 5, 6, 10, 12, 15, 20 or 30 minutes"
        );
        assert_eq!(
            message("every 5h"),
            "5 doesn't divide 24, so cron can't run it evenly (*/5 restarts every day); \
             use 1, 2, 3, 4, 6, 8 or 12 hours"
        );
        assert_eq!(
            message("every 60m"),
            "every 60 minutes is too long for */N; use 'every 1h'"
        );
        assert_eq!(
            message("every 24h"),
            "every 24 hours is too long for */N; use 'daily at 00:00'"
        );
        assert_eq!(message("daily at 25:00"), "hour 25 is out of range 0-23");
    }
}
