//! Standard 5-field cron schedules: parsing, validation, warnings and the
//! plain-English meaning. Nothing here schedules anything.

mod calendar;
mod describe;
mod error;
mod field;

use std::fmt;
use std::str::FromStr;

pub use error::ScheduleError;
use field::{Field, FieldKind, Item};

const ALIASES: [(&str, &str); 7] = [
    ("@hourly", "0 * * * *"),
    ("@daily", "0 0 * * *"),
    ("@midnight", "0 0 * * *"),
    ("@weekly", "0 0 * * 0"),
    ("@monthly", "0 0 1 * *"),
    ("@yearly", "0 0 1 1 *"),
    ("@annually", "0 0 1 1 *"),
];

/// A validated cron schedule. `Display` prints the normalised 5-field form:
/// aliases expanded and whitespace collapsed.
#[derive(Debug, Clone)]
pub struct Schedule {
    source: String,
    fields: [Field; 5],
}

impl Schedule {
    /// What the user typed, trimmed.
    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn describe(&self) -> String {
        describe::describe(self)
    }

    pub fn warnings(&self) -> Vec<String> {
        self.never_fires()
            .into_iter()
            .chain(self.fields.iter().filter_map(uneven_step))
            .collect()
    }

    fn never_fires(&self) -> Option<String> {
        let [_, _, dom, month, dow] = &self.fields;
        if dom.is_star() || !dow.is_star() {
            return None;
        }
        let days: Vec<u8> = dom.values().collect();
        let months: Vec<u8> = month.values().collect();
        let fires = months
            .iter()
            .any(|&m| days.iter().any(|&d| d <= calendar::max_days(m)));
        if fires {
            return None;
        }
        let (day_word, verb) = if days.len() == 1 {
            ("day", "occurs")
        } else {
            ("days", "occur")
        };
        let days: Vec<String> = days.iter().map(u8::to_string).collect();
        let months: Vec<String> = months.iter().map(|&m| calendar::month_name(m)).collect();
        Some(format!(
            "this schedule never fires: {day_word} {} never {verb} in {}",
            describe::join(&days),
            describe::join(&months)
        ))
    }
}

fn uneven_step(field: &Field) -> Option<String> {
    let kind = field.kind();
    if kind == FieldKind::DayOfMonth {
        // Month lengths vary, so no step is even across months anyway.
        return None;
    }
    let cycle = kind.star_max() - kind.min() + 1;
    let step = field.items().iter().find_map(|item| match *item {
        Item::Step { range: None, step } if !cycle.is_multiple_of(step) => Some(step),
        _ => None,
    })?;
    let runs: Vec<u8> = (kind.min()..=kind.star_max())
        .step_by(step.into())
        .collect();
    let last = *runs.last()?;
    let gap = kind.min() + cycle - last;
    let show = |v: u8| match kind {
        FieldKind::Minute => format!(":{v:02}"),
        _ => describe::value_name(kind, v),
    };
    let shown = match runs.as_slice() {
        [a, b, _, _, ..] => format!("{} {} … {}", show(*a), show(*b), show(last)),
        _ => runs.iter().map(|&v| show(v)).collect::<Vec<_>>().join(" "),
    };
    let unit = match kind {
        FieldKind::DayOfWeek => "day",
        _ => kind.name(),
    };
    Some(format!(
        "{}: */{step} does not divide {cycle} evenly; runs at {shown}, then {} again \
         ({gap}-{unit} gap)",
        kind.name(),
        show(kind.min())
    ))
}

impl FromStr for Schedule {
    type Err = ScheduleError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let source = s.trim();
        if source.is_empty() {
            return Err(ScheduleError::Empty);
        }
        let expr = if source.starts_with('@') {
            if source.eq_ignore_ascii_case("@reboot") {
                return Err(ScheduleError::Reboot);
            }
            ALIASES
                .iter()
                .find(|(alias, _)| alias.eq_ignore_ascii_case(source))
                .map(|(_, expr)| *expr)
                .ok_or_else(|| ScheduleError::UnknownAlias(source.to_string()))?
        } else {
            source
        };

        let parts: Vec<&str> = expr.split_whitespace().collect();
        match parts.len() {
            5 => {}
            6 => return Err(ScheduleError::SecondsField),
            n => return Err(ScheduleError::FieldCount(n)),
        }
        let fields: Vec<Field> = FieldKind::ALL
            .into_iter()
            .zip(parts)
            .map(|(kind, text)| {
                Field::parse(kind, text).map_err(|error| ScheduleError::Field { kind, error })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            source: source.to_string(),
            fields: fields.try_into().expect("exactly 5 fields"),
        })
    }
}

impl fmt::Display for Schedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let texts: Vec<&str> = self.fields.iter().map(Field::text).collect();
        f.write_str(&texts.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use error::FieldError;

    fn parse(s: &str) -> Schedule {
        s.parse().unwrap_or_else(|e| panic!("{s:?}: {e}"))
    }

    #[test]
    fn normalises_aliases_and_whitespace() {
        for (input, normalised) in [
            ("@hourly", "0 * * * *"),
            ("@daily", "0 0 * * *"),
            ("@midnight", "0 0 * * *"),
            ("@weekly", "0 0 * * 0"),
            ("@monthly", "0 0 1 * *"),
            ("@yearly", "0 0 1 1 *"),
            ("@ANNUALLY", "0 0 1 1 *"),
            ("  30\t2  * *   MON-FRI ", "30 2 * * MON-FRI"),
        ] {
            let schedule = parse(input);
            assert_eq!(schedule.to_string(), normalised, "{input:?}");
            assert_eq!(schedule.source(), input.trim());
        }
    }

    #[test]
    fn rejects_bad_schedules() {
        use FieldKind::*;
        let cases: &[(&str, ScheduleError)] = &[
            ("", ScheduleError::Empty),
            ("   ", ScheduleError::Empty),
            ("* * * *", ScheduleError::FieldCount(4)),
            ("every 15m", ScheduleError::FieldCount(2)),
            ("0 0 1 1 * 2030 x", ScheduleError::FieldCount(7)),
            ("0 */5 * * * *", ScheduleError::SecondsField),
            ("@reboot", ScheduleError::Reboot),
            ("@often", ScheduleError::UnknownAlias("@often".into())),
            (
                "61 * * * *",
                ScheduleError::Field {
                    kind: Minute,
                    error: FieldError::OutOfRange {
                        value: "61".into(),
                        min: 0,
                        max: 59,
                    },
                },
            ),
            (
                "0 0 * * 8",
                ScheduleError::Field {
                    kind: DayOfWeek,
                    error: FieldError::OutOfRange {
                        value: "8".into(),
                        min: 0,
                        max: 7,
                    },
                },
            ),
            (
                "0 0 ? * MON",
                ScheduleError::Field {
                    kind: DayOfMonth,
                    error: FieldError::Unsupported("?".into()),
                },
            ),
        ];
        for (input, expected) in cases {
            let err = input.parse::<Schedule>().unwrap_err();
            assert_eq!(&err, expected, "{input:?}");
        }
    }

    #[test]
    fn error_messages() {
        for (input, message) in [
            ("61 * * * *", "minute: 61 is out of range 0-59"),
            (
                "5/15 * * * *",
                "minute: '5/15' is not portable across cron implementations; write '5-59/15'",
            ),
            (
                "* * * * MON/2",
                "day of week: 'MON/2' is not portable across cron implementations; write 'MON-6/2'",
            ),
            ("* 5-1 * * *", "hour: range start 5 is greater than end 1"),
            ("*/0 * * * *", "minute: step 0 must be between 1 and 59"),
            ("1,,2 * * * *", "minute: empty list item"),
            (
                "0 0 L * *",
                "day of month: 'L' is Quartz syntax, which standard cron does not support",
            ),
        ] {
            let err = input.parse::<Schedule>().unwrap_err();
            assert_eq!(err.to_string(), message, "{input:?}");
        }
    }

    #[test]
    fn never_fires_warning() {
        assert_eq!(
            parse("0 0 30 2 *").warnings(),
            ["this schedule never fires: day 30 never occurs in February"]
        );
        assert_eq!(
            parse("0 0 31 4,6,9,11 *").warnings(),
            [
                "this schedule never fires: day 31 never occurs in April, June, September and November"
            ]
        );
        assert_eq!(
            parse("0 0 30,31 2 *").warnings(),
            ["this schedule never fires: days 30 and 31 never occur in February"]
        );
        for fires in ["0 0 29 2 *", "0 0 31 1-4 *", "0 0 30 2 MON", "0 0 */30 2 *"] {
            assert!(parse(fires).warnings().is_empty(), "{fires:?}");
        }
    }

    #[test]
    fn uneven_step_warning() {
        assert_eq!(
            parse("*/7 * * * *").warnings(),
            [
                "minute: */7 does not divide 60 evenly; runs at :00 :07 … :56, then :00 again (4-minute gap)"
            ]
        );
        assert_eq!(
            parse("0 */5 * * *").warnings(),
            ["hour: */5 does not divide 24 evenly; runs at 0 5 … 20, then 0 again (4-hour gap)"]
        );
        assert_eq!(
            parse("0 0 1 */5 *").warnings(),
            [
                "month: */5 does not divide 12 evenly; runs at January June November, then January again (2-month gap)"
            ]
        );
        assert_eq!(
            parse("*/45 * * * *").warnings(),
            [
                "minute: */45 does not divide 60 evenly; runs at :00 :45, then :00 again (15-minute gap)"
            ]
        );
        for even in [
            "*/5 * * * *",
            "0 */6 * * *",
            "0 0 */2 * *",
            "0-50/7 * * * *",
        ] {
            assert!(parse(even).warnings().is_empty(), "{even:?}");
        }
    }
}
