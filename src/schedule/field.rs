use super::error::FieldError;

const MONTH_NAMES: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
const WEEKDAY_NAMES: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Minute,
    Hour,
    DayOfMonth,
    Month,
    DayOfWeek,
}

impl FieldKind {
    /// In the order they appear in a cron expression.
    pub const ALL: [FieldKind; 5] = [
        Self::Minute,
        Self::Hour,
        Self::DayOfMonth,
        Self::Month,
        Self::DayOfWeek,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Minute => "minute",
            Self::Hour => "hour",
            Self::DayOfMonth => "day of month",
            Self::Month => "month",
            Self::DayOfWeek => "day of week",
        }
    }

    pub fn min(self) -> u8 {
        match self {
            Self::DayOfMonth | Self::Month => 1,
            _ => 0,
        }
    }

    pub fn max(self) -> u8 {
        match self {
            Self::Minute => 59,
            Self::Hour => 23,
            Self::DayOfMonth => 31,
            Self::Month => 12,
            Self::DayOfWeek => 7,
        }
    }

    /// The last value `*` stands for: day of week 7 is another name for 0.
    pub fn star_max(self) -> u8 {
        match self {
            Self::DayOfWeek => 6,
            _ => self.max(),
        }
    }

    fn names(self) -> &'static [&'static str] {
        match self {
            Self::Month => &MONTH_NAMES,
            Self::DayOfWeek => &WEEKDAY_NAMES,
            _ => &[],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    All,
    Value(u8),
    Range(u8, u8),
    Step { range: Option<(u8, u8)>, step: u8 },
}

/// One parsed field: the items as written (for the description) and the
/// expanded set of values (for checks, and later for next fire times).
#[derive(Debug, Clone)]
pub struct Field {
    kind: FieldKind,
    text: String,
    items: Vec<Item>,
    values: u64,
}

impl Field {
    pub fn parse(kind: FieldKind, text: &str) -> Result<Self, FieldError> {
        let items = text
            .split(',')
            .map(|item| parse_item(kind, item))
            .collect::<Result<Vec<_>, _>>()?;
        let values = items.iter().fold(0, |acc, item| acc | expand(kind, item));
        Ok(Self {
            kind,
            text: text.to_string(),
            items,
            values,
        })
    }

    pub fn kind(&self) -> FieldKind {
        self.kind
    }

    /// The field exactly as the user wrote it.
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn is_all(&self) -> bool {
        self.items == [Item::All]
    }

    /// Whether the field starts with `*`. Cron uses this, not the value set,
    /// to decide whether day of month and day of week are AND-ed or OR-ed.
    pub fn is_star(&self) -> bool {
        self.text.starts_with('*')
    }

    pub fn contains(&self, value: u8) -> bool {
        self.values & (1 << fold(self.kind, value)) != 0
    }

    /// The expanded values in ascending order (day of week 7 is folded into 0).
    pub fn values(&self) -> impl Iterator<Item = u8> + '_ {
        (self.kind.min()..=self.kind.star_max()).filter(|&v| self.contains(v))
    }
}

fn parse_item(kind: FieldKind, item: &str) -> Result<Item, FieldError> {
    if item.is_empty() {
        return Err(FieldError::EmptyItem);
    }
    let (base, step) = match item.split_once('/') {
        Some((base, step)) => (base, Some(step)),
        None => (item, None),
    };
    let range = if base == "*" {
        None
    } else if let Some((start, end)) = base.split_once('-') {
        let (start, end) = (
            parse_value(kind, start, item)?,
            parse_value(kind, end, item)?,
        );
        if start > end {
            return Err(FieldError::ReversedRange { start, end });
        }
        Some((start, end))
    } else {
        let value = parse_value(kind, base, item)?;
        return match step {
            None => Ok(Item::Value(value)),
            Some(step) => Err(FieldError::BareStep {
                item: item.to_string(),
                suggestion: format!("{base}-{}/{step}", kind.star_max()),
            }),
        };
    };
    let Some(step) = step else {
        return Ok(range.map_or(Item::All, |(start, end)| Item::Range(start, end)));
    };

    if step.is_empty() || !step.bytes().all(|b| b.is_ascii_digit()) {
        return Err(FieldError::Invalid(item.to_string()));
    }
    let (start, end) = range.unwrap_or((kind.min(), kind.star_max()));
    let max = (end - start).max(1);
    match step.parse::<u8>() {
        Ok(n) if (1..=max).contains(&n) => Ok(Item::Step { range, step: n }),
        _ => Err(FieldError::BadStep {
            step: step.to_string(),
            max,
        }),
    }
}

fn parse_value(kind: FieldKind, token: &str, item: &str) -> Result<u8, FieldError> {
    if !token.is_empty() && token.bytes().all(|b| b.is_ascii_digit()) {
        return match token.parse::<u8>() {
            Ok(v) if (kind.min()..=kind.max()).contains(&v) => Ok(v),
            _ => Err(FieldError::OutOfRange {
                value: token.to_string(),
                min: kind.min(),
                max: kind.max(),
            }),
        };
    }
    if let Some(i) = kind
        .names()
        .iter()
        .position(|name| name.eq_ignore_ascii_case(token))
    {
        return Ok(i as u8 + kind.min());
    }
    if is_quartz(token) {
        return Err(FieldError::Unsupported(token.to_string()));
    }
    Err(FieldError::Invalid(item.to_string()))
}

/// `?`, `5#3`, `L`, `LW`, `15W`, `5L`.
fn is_quartz(token: &str) -> bool {
    let core = token.trim_end_matches(['L', 'l', 'W', 'w']);
    token == "?"
        || token.contains('#')
        || (core.len() < token.len() && core.bytes().all(|b| b.is_ascii_digit()))
}

fn expand(kind: FieldKind, item: &Item) -> u64 {
    let (start, end, step) = match *item {
        Item::All => (kind.min(), kind.star_max(), 1),
        Item::Value(v) => (v, v, 1),
        Item::Range(start, end) => (start, end, 1),
        Item::Step { range, step } => {
            let (start, end) = range.unwrap_or((kind.min(), kind.star_max()));
            (start, end, step)
        }
    };
    (start..=end)
        .step_by(step.into())
        .fold(0, |acc, v| acc | 1 << fold(kind, v))
}

fn fold(kind: FieldKind, value: u8) -> u8 {
    if kind == FieldKind::DayOfWeek && value == 7 {
        0
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use FieldKind::*;

    fn values(kind: FieldKind, text: &str) -> Vec<u8> {
        Field::parse(kind, text).unwrap().values().collect()
    }

    #[test]
    fn expands_values() {
        assert_eq!(values(Minute, "*/15"), [0, 15, 30, 45]);
        assert_eq!(values(Minute, "0-30/10"), [0, 10, 20, 30]);
        assert_eq!(values(Hour, "1,3-5"), [1, 3, 4, 5]);
        assert_eq!(values(DayOfMonth, "*/10"), [1, 11, 21, 31]);
        assert_eq!(values(Month, "jan,Jul-SEP"), [1, 7, 8, 9]);
        assert_eq!(values(DayOfWeek, "MON-FRI"), [1, 2, 3, 4, 5]);
        assert_eq!(values(DayOfWeek, "*"), [0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn day_of_week_7_is_sunday() {
        assert_eq!(values(DayOfWeek, "0,7"), values(DayOfWeek, "0"));
        assert_eq!(values(DayOfWeek, "5-7"), [0, 5, 6]);
    }

    #[test]
    fn keeps_item_structure() {
        let field = Field::parse(Minute, "*,5,1-3,*/15,0-30/10").unwrap();
        assert_eq!(
            field.items(),
            [
                Item::All,
                Item::Value(5),
                Item::Range(1, 3),
                Item::Step {
                    range: None,
                    step: 15
                },
                Item::Step {
                    range: Some((0, 30)),
                    step: 10
                },
            ]
        );
    }

    #[test]
    fn rejects_bad_fields() {
        use FieldError::*;
        let cases: &[(FieldKind, &str, FieldError)] = &[
            (Minute, "1,,2", EmptyItem),
            (Minute, "abc", Invalid("abc".into())),
            (Minute, "5-", Invalid("5-".into())),
            (Minute, "*/x", Invalid("*/x".into())),
            (Hour, "MON", Invalid("MON".into())),
            (
                Minute,
                "61",
                OutOfRange {
                    value: "61".into(),
                    min: 0,
                    max: 59,
                },
            ),
            (
                DayOfMonth,
                "0",
                OutOfRange {
                    value: "0".into(),
                    min: 1,
                    max: 31,
                },
            ),
            (
                Minute,
                "99999",
                OutOfRange {
                    value: "99999".into(),
                    min: 0,
                    max: 59,
                },
            ),
            (Hour, "5-1", ReversedRange { start: 5, end: 1 }),
            (
                Minute,
                "*/0",
                BadStep {
                    step: "0".into(),
                    max: 59,
                },
            ),
            (
                Minute,
                "0-10/11",
                BadStep {
                    step: "11".into(),
                    max: 10,
                },
            ),
            (
                Minute,
                "5/15",
                BareStep {
                    item: "5/15".into(),
                    suggestion: "5-59/15".into(),
                },
            ),
            (DayOfMonth, "?", Unsupported("?".into())),
            (DayOfMonth, "L", Unsupported("L".into())),
            (DayOfMonth, "15W", Unsupported("15W".into())),
            (DayOfWeek, "5#3", Unsupported("5#3".into())),
            (DayOfWeek, "5L", Unsupported("5L".into())),
        ];
        for (kind, text, expected) in cases {
            let err = Field::parse(*kind, text).unwrap_err();
            assert_eq!(&err, expected, "{kind:?} {text:?}");
        }
    }
}
