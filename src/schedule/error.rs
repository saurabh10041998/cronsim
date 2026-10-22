use std::error::Error;
use std::fmt;

use super::field::FieldKind;
use super::human::HumanError;

/// Why a schedule was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleError {
    Empty,
    FieldCount(usize),
    /// Six fields, which usually means a leading seconds field (Quartz, Spring, …).
    SecondsField,
    Reboot,
    UnknownAlias(String),
    Human(HumanError),
    Field {
        kind: FieldKind,
        error: FieldError,
    },
}

/// Why a single field was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldError {
    EmptyItem,
    Invalid(String),
    OutOfRange {
        value: String,
        min: u8,
        max: u8,
    },
    ReversedRange {
        start: u8,
        end: u8,
    },
    BadStep {
        step: String,
        max: u8,
    },
    /// `A/S`: Vixie cron reads it as `A-max/S`, other crons reject it.
    BareStep {
        item: String,
        suggestion: String,
    },
    /// Quartz-only syntax: `?`, `L`, `W`, `#`.
    Unsupported(String),
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("schedule is empty"),
            Self::FieldCount(n) => write!(
                f,
                "expected 5 fields (minute hour day-of-month month day-of-week), found {n}"
            ),
            Self::SecondsField => f.write_str(
                "expected 5 fields, found 6; this looks like a seconds field, \
                 but cron's smallest unit is one minute",
            ),
            Self::Reboot => f.write_str("@reboot is not a time-based schedule"),
            Self::UnknownAlias(alias) => write!(
                f,
                "unknown alias '{alias}'; expected one of @hourly, @daily, @midnight, \
                 @weekly, @monthly, @yearly, @annually"
            ),
            Self::Human(error) => error.fmt(f),
            Self::Field { kind, error } => write!(f, "{}: {error}", kind.name()),
        }
    }
}

impl fmt::Display for FieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyItem => f.write_str("empty list item"),
            Self::Invalid(item) => write!(f, "invalid value '{item}'"),
            Self::OutOfRange { value, min, max } => {
                write!(f, "{value} is out of range {min}-{max}")
            }
            Self::ReversedRange { start, end } => {
                write!(f, "range start {start} is greater than end {end}")
            }
            Self::BadStep { step, max } => write!(f, "step {step} must be between 1 and {max}"),
            Self::BareStep { item, suggestion } => write!(
                f,
                "'{item}' is not portable across cron implementations; write '{suggestion}'"
            ),
            Self::Unsupported(token) => write!(
                f,
                "'{token}' is Quartz syntax, which standard cron does not support"
            ),
        }
    }
}

impl Error for ScheduleError {}
impl Error for FieldError {}
