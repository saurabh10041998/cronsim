//! Text shared by the commands that print jobs and runs.

use jiff::Timestamp;
use jiff::tz::TimeZone;
use time::OffsetDateTime;

use crate::history::{Outcome, SkipReason};
use crate::job::Command;

pub const TIME_FORMAT: &str = "%a %Y-%m-%d %H:%M";

/// A history time in `tz`, in `TIME_FORMAT`.
pub fn local_time(t: OffsetDateTime, tz: &TimeZone) -> String {
    match Timestamp::new(t.unix_timestamp(), t.nanosecond() as i32) {
        Ok(ts) => ts.to_zoned(tz.clone()).strftime(TIME_FORMAT).to_string(),
        Err(_) => t.to_string(),
    }
}

/// Rounded down to the largest whole unit: `41s`, `12m`, `1h`.
pub fn duration(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
}

/// Like `duration`, but only rounds when nothing is lost: `90s`, `2m`, `1h`.
pub fn exact_duration(secs: u64) -> String {
    match secs {
        s if s >= 3600 && s % 3600 == 0 => format!("{}h", s / 3600),
        s if s >= 60 && s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

pub fn outcome_text(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Exited(0) => "ok".to_string(),
        Outcome::Exited(code) => format!("exit {code}"),
        Outcome::Signaled(signal) => format!("signal {signal}"),
        Outcome::TimedOut => "timed out".to_string(),
        Outcome::Skipped(SkipReason::Overlap) => "skipped (overlap)".to_string(),
        Outcome::SpawnFailed(_) => "failed to start".to_string(),
    }
}

/// Linux numbering.
pub fn signal_name(signal: i32) -> Option<&'static str> {
    Some(match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        6 => "SIGABRT",
        9 => "SIGKILL",
        10 => "SIGUSR1",
        11 => "SIGSEGV",
        12 => "SIGUSR2",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        _ => return None,
    })
}

pub fn shell_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b));
    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

pub fn argv_line<S: AsRef<str>>(argv: &[S]) -> String {
    argv.iter()
        .map(|arg| shell_quote(arg.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn command_line(command: &Command) -> String {
    match command {
        Command::Exec { program, args } => {
            argv_line(&std::iter::once(program).chain(args).collect::<Vec<_>>())
        }
        Command::Script { path } => shell_quote(&path.to_string_lossy()),
    }
}

/// Left-aligned columns three spaces apart, padded to the widest cell.
pub fn table<const N: usize>(rows: &[[String; N]]) -> Vec<String> {
    let mut widths = [0; N];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    rows.iter()
        .map(|row| {
            let mut line = String::new();
            for (cell, width) in row.iter().zip(widths) {
                line.push_str(&format!("{cell:width$}   "));
            }
            line.trim_end().to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn durations() {
        for (secs, text) in [
            (-1, "0s"),
            (0, "0s"),
            (59, "59s"),
            (60, "1m"),
            (3599, "59m"),
            (3660, "1h"),
            (90000, "25h"),
        ] {
            assert_eq!(duration(secs), text, "{secs}");
        }
    }

    #[test]
    fn exact_durations() {
        for (secs, text) in [
            (1, "1s"),
            (60, "1m"),
            (90, "90s"),
            (3600, "1h"),
            (5400, "90m"),
            (3601, "3601s"),
        ] {
            assert_eq!(exact_duration(secs), text, "{secs}");
        }
    }

    #[test]
    fn signal_names() {
        assert_eq!(signal_name(9), Some("SIGKILL"));
        assert_eq!(signal_name(15), Some("SIGTERM"));
        assert_eq!(signal_name(64), None);
    }

    #[test]
    fn command_lines() {
        let exec = Command::Exec {
            program: "sh".into(),
            args: vec!["-c".into(), "echo 'hi' $HOME".into(), String::new()],
        };
        assert_eq!(command_line(&exec), r#"sh -c 'echo '\''hi'\'' $HOME' ''"#);
        let script = Command::Script {
            path: PathBuf::from("/opt/my jobs/b.sh"),
        };
        assert_eq!(command_line(&script), "'/opt/my jobs/b.sh'");
    }

    #[test]
    fn table_alignment() {
        let rows = [
            ["a".to_string(), "bb".into(), "c".into()],
            ["aaa".into(), "b".into(), String::new()],
        ];
        assert_eq!(table(&rows), ["a     bb   c", "aaa   b"]);
    }
}
