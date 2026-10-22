//! The plain-English meaning of a schedule, close to crontab.guru's style.
//!
//! The sentence is a time part (minute + hour), then a day part (day of
//! month, month, day of week).

use super::Schedule;
use super::field::{Field, FieldKind, Item};

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
const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// Above this many clock times, `At 09:00, 10:00, …` is spelled per field instead.
const MAX_CLOCK_TIMES: usize = 4;

pub(super) fn describe(schedule: &Schedule) -> String {
    let [minute, hour, dom, month, dow] = &schedule.fields;
    let (time, is_clock) = time_part(minute, hour);
    match day_part(dom, month, dow) {
        Some(days) => format!("{time}, {days}"),
        None if is_clock => format!("{time}, every day"),
        None => time,
    }
}

/// The text, and whether it names exact clock times (`At 02:30`).
fn time_part(minute: &Field, hour: &Field) -> (String, bool) {
    if let (Some(minutes), Some(hours)) = (plain_values(minute), plain_values(hour))
        && minutes.len() * hours.len() <= MAX_CLOCK_TIMES
    {
        let times: Vec<String> = hours
            .iter()
            .flat_map(|h| minutes.iter().map(move |m| format!("{h:02}:{m:02}")))
            .collect();
        return (format!("At {}", join(&times)), true);
    }

    let minutes = match minute.items() {
        [Item::All] => "Every minute".to_string(),
        [step @ Item::Step { .. }] => capitalize(&item_text(FieldKind::Minute, step)),
        items => format!(
            "At {} {}",
            plural("minute", items),
            list(FieldKind::Minute, items)
        ),
    };
    let text = match hour.items() {
        [Item::All] => minutes,
        [step @ Item::Step { .. }] => {
            format!("{minutes} past {}", item_text(FieldKind::Hour, step))
        }
        items => format!(
            "{minutes} past {} {}",
            plural("hour", items),
            list(FieldKind::Hour, items)
        ),
    };
    (text, false)
}

/// `None` when every day matches.
fn day_part(dom: &Field, month: &Field, dow: &Field) -> Option<String> {
    let on_dom = match dom.items() {
        [step @ Item::Step { .. }] => {
            format!("on {} of the month", item_text(FieldKind::DayOfMonth, step))
        }
        items => format!(
            "on {} {} of the month",
            plural("day", items),
            list(FieldKind::DayOfMonth, items)
        ),
    };
    let on_dow = format!("on {}", list(FieldKind::DayOfWeek, dow.items()));

    let days = match (dom.is_all(), dow.is_all()) {
        (true, true) => None,
        (false, true) => Some(on_dom),
        (true, false) => Some(match dow.items() {
            [range @ Item::Range(..)] => item_text(FieldKind::DayOfWeek, range),
            _ => on_dow,
        }),
        // Both restricted: cron ANDs them if either starts with `*`, else ORs them.
        (false, false) if dom.is_star() || dow.is_star() => {
            Some(format!("{on_dom}, but only {on_dow}"))
        }
        (false, false) => Some(format!("{on_dom} or {on_dow}")),
    };
    if month.is_all() {
        return days;
    }
    let in_month = format!("in {}", list(FieldKind::Month, month.items()));
    Some(match days {
        Some(days) => format!("{days} {in_month}"),
        None => in_month,
    })
}

/// The values, if the field is only a list of single values.
fn plain_values(field: &Field) -> Option<Vec<u8>> {
    let plain = field
        .items()
        .iter()
        .all(|item| matches!(item, Item::Value(_)));
    plain.then(|| field.values().collect())
}

fn item_text(kind: FieldKind, item: &Item) -> String {
    let unit = match kind {
        FieldKind::DayOfMonth => "day",
        FieldKind::DayOfWeek => "day of the week",
        _ => kind.name(),
    };
    match *item {
        Item::All => format!("every {unit}"),
        Item::Value(v) => value_name(kind, v),
        Item::Range(start, end) => format!(
            "{} through {}",
            value_name(kind, start),
            value_name(kind, end)
        ),
        Item::Step { range, step } => {
            let every = if step == 1 {
                format!("every {unit}")
            } else {
                format!("every {} {unit}", ordinal(step))
            };
            match range {
                None => every,
                Some((start, end)) => format!(
                    "{every} from {} through {}",
                    value_name(kind, start),
                    value_name(kind, end)
                ),
            }
        }
    }
}

fn list(kind: FieldKind, items: &[Item]) -> String {
    let texts: Vec<String> = items.iter().map(|item| item_text(kind, item)).collect();
    join(&texts)
}

pub(super) fn value_name(kind: FieldKind, value: u8) -> String {
    match kind {
        FieldKind::Month => MONTHS[usize::from(value) - 1].to_string(),
        FieldKind::DayOfWeek => WEEKDAYS[usize::from(value % 7)].to_string(),
        _ => value.to_string(),
    }
}

/// `a`, `a and b`, `a, b and c`.
pub(super) fn join(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

fn plural(word: &str, items: &[Item]) -> String {
    match items {
        [Item::Value(_)] => word.to_string(),
        _ => format!("{word}s"),
    }
}

fn ordinal(n: u8) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The golden table: one row per case in the plan's §3.5, plus aliases,
    /// names and edge cases.
    #[test]
    fn meanings() {
        for (input, meaning) in [
            // Time part.
            ("30 2 * * *", "At 02:30, every day"),
            ("0 9,17 * * *", "At 09:00 and 17:00, every day"),
            (
                "0,30 9,17 * * *",
                "At 09:00, 09:30, 17:00 and 17:30, every day",
            ),
            (
                "0 1,2,3,4,5 * * *",
                "At minute 0 past hours 1, 2, 3, 4 and 5",
            ),
            ("* * * * *", "Every minute"),
            ("*/1 * * * *", "Every minute"),
            ("*/15 * * * *", "Every 15th minute"),
            ("*/2 * * * *", "Every 2nd minute"),
            ("0-30/10 * * * *", "Every 10th minute from 0 through 30"),
            ("0 */2 * * *", "At minute 0 past every 2nd hour"),
            (
                "0 9-17/2 * * *",
                "At minute 0 past every 2nd hour from 9 through 17",
            ),
            (
                "5,10 1-3 * * *",
                "At minutes 5 and 10 past hours 1 through 3",
            ),
            ("5 * * * *", "At minute 5"),
            ("* 9 * * *", "Every minute past hour 9"),
            (
                "*/15 9-17 * * *",
                "Every 15th minute past hours 9 through 17",
            ),
            // Day part.
            ("30 2 * * 1-5", "At 02:30, Monday through Friday"),
            ("30 2 * * MON-FRI", "At 02:30, Monday through Friday"),
            ("0 9 * * 1,3", "At 09:00, on Monday and Wednesday"),
            ("0 9 * * 7", "At 09:00, on Sunday"),
            ("0 9 * * 5-7", "At 09:00, Friday through Sunday"),
            ("0 0 1 * *", "At 00:00, on day 1 of the month"),
            ("0 0 1,15 * *", "At 00:00, on days 1 and 15 of the month"),
            ("0 0 */2 * *", "At 00:00, on every 2nd day of the month"),
            ("0 0 * 1,7 *", "At 00:00, in January and July"),
            ("0 0 * jan-mar *", "At 00:00, in January through March"),
            ("0 0 1 * 1", "At 00:00, on day 1 of the month or on Monday"),
            (
                "0 0 */2 * 1-5",
                "At 00:00, on every 2nd day of the month, but only on Monday through Friday",
            ),
            ("0 0 1 6 *", "At 00:00, on day 1 of the month in June"),
            ("*/5 * * * 0", "Every 5th minute, on Sunday"),
            // Aliases.
            ("@hourly", "At minute 0"),
            ("@daily", "At 00:00, every day"),
            ("@weekly", "At 00:00, on Sunday"),
            ("@monthly", "At 00:00, on day 1 of the month"),
            ("@yearly", "At 00:00, on day 1 of the month in January"),
        ] {
            let schedule: Schedule = input.parse().unwrap();
            assert_eq!(schedule.describe(), meaning, "{input:?}");
        }
    }

    #[test]
    fn helpers() {
        let ordinals: Vec<String> = [1, 2, 3, 4, 11, 12, 13, 21, 22, 23, 111]
            .into_iter()
            .map(ordinal)
            .collect();
        assert_eq!(
            ordinals,
            [
                "1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd", "23rd", "111th"
            ]
        );
        let words = |w: &[&str]| join(&w.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(words(&[]), "");
        assert_eq!(words(&["a"]), "a");
        assert_eq!(words(&["a", "b"]), "a and b");
        assert_eq!(words(&["a", "b", "c"]), "a, b and c");
    }
}
