//! `cronsim list`: every job with its schedule, next run and last result.

use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};
use serde::Serialize;
use time::OffsetDateTime;

use crate::history::{Outcome, RunRecord, SkipReason, Trigger};
use crate::schedule::Schedule;
use crate::store::{Store, StoreError};

pub struct Entry {
    pub name: String,
    pub job: Result<JobView, String>,
}

pub struct JobView {
    pub schedule: Schedule,
    pub paused: bool,
    /// `None` when paused or the schedule never fires.
    pub next_run: Option<Zoned>,
    pub last_run: Result<Option<RunRecord>, String>,
}

impl Entry {
    pub fn errors(&self) -> Vec<&str> {
        match &self.job {
            Err(e) => vec![e],
            Ok(view) => view
                .last_run
                .as_ref()
                .err()
                .map(String::as_str)
                .into_iter()
                .collect(),
        }
    }
}

pub fn entries(store: &Store, now: &Zoned) -> Result<Vec<Entry>, StoreError> {
    Ok(store
        .list_jobs()?
        .into_iter()
        .map(|(name, spec)| {
            let job = spec.map_err(|e| e.to_string()).map(|spec| JobView {
                next_run: if spec.paused {
                    None
                } else {
                    spec.schedule.next_runs(now, 1).into_iter().next()
                },
                paused: spec.paused,
                last_run: store.last_run(&name).map_err(|e| e.to_string()),
                schedule: spec.schedule,
            });
            Entry { name, job }
        })
        .collect())
}

const TIME_FORMAT: &str = "%a %Y-%m-%d %H:%M";

pub fn render_table(entries: &[Entry], tz: &TimeZone, zone: &str) -> String {
    let mut rows = vec![["NAME", "SCHEDULE", "NEXT RUN", "LAST RESULT"].map(String::from)];
    for entry in entries {
        let row = match &entry.job {
            Err(_) => [
                entry.name.clone(),
                "(error)".into(),
                String::new(),
                String::new(),
            ],
            Ok(view) => [
                entry.name.clone(),
                view.schedule
                    .typed_form()
                    .map_or_else(|| view.schedule.to_string(), String::from),
                match (&view.next_run, view.paused) {
                    (_, true) => "paused".into(),
                    (Some(next), false) => next.strftime(TIME_FORMAT).to_string(),
                    (None, false) => "never".into(),
                },
                match &view.last_run {
                    Err(_) => "history unreadable".into(),
                    Ok(None) => "never run".into(),
                    Ok(Some(run)) => last_result(run, tz),
                },
            ],
        };
        rows.push(row);
    }

    let mut widths = [0; 4];
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in &rows {
        let mut line = String::new();
        for (cell, width) in row.iter().zip(widths) {
            line.push_str(&format!("{cell:width$}   "));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.push_str(&format!("time zone: {zone}\n"));
    out
}

fn last_result(run: &RunRecord, tz: &TimeZone) -> String {
    let outcome = match &run.outcome {
        Outcome::Exited(0) => "ok".to_string(),
        Outcome::Exited(code) => format!("exit {code}"),
        Outcome::Signaled(signal) => format!("signal {signal}"),
        Outcome::TimedOut => "timed out".to_string(),
        Outcome::Skipped(SkipReason::Overlap) => "skipped (overlap)".to_string(),
        Outcome::SpawnFailed(_) => "failed to start".to_string(),
    };
    let started = match to_jiff(run.started) {
        Some(ts) => ts.to_zoned(tz.clone()).strftime(TIME_FORMAT).to_string(),
        None => run.started.to_string(),
    };
    let mut text = format!(
        "{outcome}, {started} ({})",
        duration((run.finished - run.started).whole_seconds())
    );
    if run.trigger == Trigger::Manual {
        text.push_str(", manual");
    }
    text
}

fn to_jiff(t: OffsetDateTime) -> Option<Timestamp> {
    Timestamp::new(t.unix_timestamp(), t.nanosecond() as i32).ok()
}

/// Rounded down to the largest whole unit: `41s`, `12m`, `1h`.
fn duration(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
}

#[derive(Serialize)]
struct JsonList<'a> {
    jobs: Vec<JsonEntry<'a>>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum JsonEntry<'a> {
    Job {
        name: &'a str,
        schedule: &'a Schedule,
        paused: bool,
        next_run: Option<String>,
        last_run: Option<&'a RunRecord>,
        #[serde(skip_serializing_if = "Option::is_none")]
        history_error: Option<&'a str>,
    },
    Broken {
        name: &'a str,
        error: &'a str,
    },
}

/// Times are RFC 3339 in UTC, like the run history.
pub fn to_json(entries: &[Entry]) -> String {
    let jobs = entries
        .iter()
        .map(|entry| match &entry.job {
            Err(error) => JsonEntry::Broken {
                name: &entry.name,
                error,
            },
            Ok(view) => JsonEntry::Job {
                name: &entry.name,
                schedule: &view.schedule,
                paused: view.paused,
                next_run: view.next_run.as_ref().map(|t| t.timestamp().to_string()),
                last_run: view.last_run.as_ref().ok().and_then(Option::as_ref),
                history_error: view.last_run.as_ref().err().map(String::as_str),
            },
        })
        .collect();
    let mut text =
        serde_json::to_string_pretty(&JsonList { jobs }).expect("job list serialises to JSON");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::history::RunId;
    use crate::job::{Command, JobSpec};
    use crate::store::tests::store;

    fn now(tz: &str) -> Zoned {
        let tz = jiff::tz::TimeZoneDatabase::bundled().get(tz).unwrap();
        "2026-10-26T10:07:00Z"
            .parse::<Timestamp>()
            .unwrap()
            .to_zoned(tz)
    }

    fn at(stamp: &str) -> OffsetDateTime {
        let ts: Timestamp = stamp.parse().unwrap();
        OffsetDateTime::from_unix_timestamp(ts.as_second()).unwrap()
    }

    fn spec(name: &str, schedule: &str) -> JobSpec {
        JobSpec::new(
            name.into(),
            schedule.parse().unwrap(),
            Command::Exec {
                program: "true".into(),
                args: vec![],
            },
        )
    }

    fn run(job: &str, started: &str, secs: i64, outcome: Outcome, trigger: Trigger) -> RunRecord {
        let started = at(started);
        RunRecord::new(
            job,
            RunId::new(started, 4242),
            started,
            started + time::Duration::seconds(secs),
            outcome,
            trigger,
        )
        .unwrap()
    }

    #[test]
    fn entries_have_next_and_last_run() {
        let (_dir, store) = store();
        store.save_job(&spec("report", "daily at 02:30")).unwrap();
        store.save_job(&spec("backup", "every 15m")).unwrap();
        let mut paused = spec("paused", "@hourly");
        paused.paused = true;
        store.save_job(&paused).unwrap();
        store.save_job(&spec("never", "0 0 30 2 *")).unwrap();
        let first = run(
            "backup",
            "2026-10-26T09:45:00Z",
            3,
            Outcome::Exited(0),
            Trigger::Cron,
        );
        let last = run(
            "backup",
            "2026-10-26T10:00:00Z",
            3,
            Outcome::Exited(1),
            Trigger::Cron,
        );
        let old = run(
            "paused",
            "2026-10-20T08:00:00Z",
            3,
            Outcome::TimedOut,
            Trigger::Cron,
        );
        for record in [&first, &last, &old] {
            store.append_run(record).unwrap();
        }

        let entries = entries(&store, &now("UTC")).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["backup", "never", "paused", "report"]);
        let view = |i: usize| entries[i].job.as_ref().unwrap();
        let next = |i: usize| view(i).next_run.as_ref().map(|t| t.timestamp().to_string());

        assert_eq!(next(0).as_deref(), Some("2026-10-26T10:15:00Z"));
        assert_eq!(view(0).last_run, Ok(Some(last)));
        assert_eq!(next(1), None);
        assert_eq!(view(1).last_run, Ok(None));
        assert_eq!(next(2), None);
        assert!(view(2).paused);
        assert_eq!(view(2).last_run, Ok(Some(old)));
        assert_eq!(next(3).as_deref(), Some("2026-10-27T02:30:00Z"));
        assert!(entries.iter().all(|e| e.errors().is_empty()));
    }

    #[test]
    fn next_run_uses_the_given_zone() {
        let (_dir, store) = store();
        store.save_job(&spec("report", "daily at 02:30")).unwrap();
        let entries = entries(&store, &now("Europe/Berlin")).unwrap();
        let next = entries[0].job.as_ref().unwrap().next_run.as_ref().unwrap();
        assert_eq!(next.timestamp().to_string(), "2026-10-27T01:30:00Z");
    }

    #[test]
    fn unreadable_spec_and_history_are_reported() {
        let (_dir, store) = store();
        store.save_job(&spec("a", "@daily")).unwrap();
        store.save_job(&spec("c", "@daily")).unwrap();
        fs::write(store.root().join("jobs/b.toml"), "???").unwrap();
        fs::write(store.root().join("runs/c.jsonl"), "garbage\n").unwrap();

        let entries = entries(&store, &now("UTC")).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert!(entries[0].errors().is_empty());
        let [spec_error] = entries[1].errors()[..] else {
            panic!("one error expected");
        };
        assert!(spec_error.contains("b.toml"), "{spec_error}");
        let [history_error] = entries[2].errors()[..] else {
            panic!("one error expected");
        };
        assert!(history_error.contains("c.jsonl:1:"), "{history_error}");
    }

    #[test]
    fn missing_jobs_dir_is_an_error() {
        let (_dir, store) = store();
        fs::remove_dir(store.root().join("jobs")).unwrap();
        assert!(matches!(
            entries(&store, &now("UTC")),
            Err(StoreError::Io { .. })
        ));
    }

    fn view(
        schedule: &str,
        next_run: Option<&str>,
        last_run: Result<Option<RunRecord>, String>,
    ) -> JobView {
        JobView {
            schedule: schedule.parse().unwrap(),
            paused: false,
            next_run: next_run.map(|t| t.parse::<Timestamp>().unwrap().to_zoned(TimeZone::UTC)),
            last_run,
        }
    }

    fn entry(name: &str, job: Result<JobView, String>) -> Entry {
        Entry {
            name: name.into(),
            job,
        }
    }

    #[test]
    fn table_layout() {
        let next = Some("2026-10-26T10:15:00Z");
        let mut paused = view("*/7 * * * *", next, Ok(None));
        paused.paused = true;
        let entries = [
            entry(
                "backup",
                Ok(view(
                    "every 15m",
                    next,
                    Ok(Some(run(
                        "backup",
                        "2026-10-26T10:00:00Z",
                        3,
                        Outcome::Exited(0),
                        Trigger::Cron,
                    ))),
                )),
            ),
            entry("broken", Err("bad".into())),
            entry(
                "report",
                Ok(view(
                    "30  2 * * *",
                    Some("2026-10-27T02:30:00Z"),
                    Ok(Some(run(
                        "report",
                        "2026-10-26T02:30:00Z",
                        41,
                        Outcome::Exited(2),
                        Trigger::Manual,
                    ))),
                )),
            ),
            entry("sync", Ok(paused)),
            entry("x", Ok(view("0 0 30 2 *", None, Err("bad".into())))),
        ];
        assert_eq!(
            render_table(&entries, &TimeZone::UTC, "UTC (system local)"),
            "\
NAME     SCHEDULE      NEXT RUN               LAST RESULT
backup   every 15m     Mon 2026-10-26 10:15   ok, Mon 2026-10-26 10:00 (3s)
broken   (error)
report   30 2 * * *    Tue 2026-10-27 02:30   exit 2, Mon 2026-10-26 02:30 (41s), manual
sync     */7 * * * *   paused                 never run
x        0 0 30 2 *    never                  history unreadable
time zone: UTC (system local)
"
        );
    }

    #[test]
    fn last_result_text() {
        let tz = jiff::tz::TimeZoneDatabase::bundled()
            .get("Europe/Berlin")
            .unwrap();
        let text = |outcome, secs| {
            last_result(
                &run("j", "2026-10-26T10:00:00Z", secs, outcome, Trigger::Cron),
                &tz,
            )
        };
        assert_eq!(text(Outcome::Exited(0), 0), "ok, Mon 2026-10-26 11:00 (0s)");
        assert_eq!(
            text(Outcome::Signaled(9), 59),
            "signal 9, Mon 2026-10-26 11:00 (59s)"
        );
        assert_eq!(
            text(Outcome::TimedOut, 3600),
            "timed out, Mon 2026-10-26 11:00 (1h)"
        );
        assert_eq!(
            text(Outcome::Skipped(SkipReason::Overlap), 0),
            "skipped (overlap), Mon 2026-10-26 11:00 (0s)"
        );
        assert_eq!(
            text(Outcome::SpawnFailed("not found".into()), 0),
            "failed to start, Mon 2026-10-26 11:00 (0s)"
        );
    }

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
    fn json_format() {
        let entries = [
            entry(
                "backup",
                Ok(view(
                    "every 15m",
                    Some("2026-10-26T10:15:00Z"),
                    Ok(Some(run(
                        "backup",
                        "2026-10-26T10:00:00Z",
                        3,
                        Outcome::Exited(0),
                        Trigger::Cron,
                    ))),
                )),
            ),
            entry("broken", Err("bad toml".into())),
            entry("x", Ok(view("0 0 30 2 *", None, Err("bad jsonl".into())))),
        ];
        assert_eq!(
            to_json(&entries),
            r#"{
  "jobs": [
    {
      "name": "backup",
      "schedule": {
        "source": "every 15m",
        "cron": "*/15 * * * *"
      },
      "paused": false,
      "next_run": "2026-10-26T10:15:00Z",
      "last_run": {
        "id": "20261026T100000Z-4242",
        "job": "backup",
        "started": "2026-10-26T10:00:00Z",
        "finished": "2026-10-26T10:00:03Z",
        "outcome": {
          "exited": 0
        },
        "log": "logs/backup/20261026T100000Z-4242.log",
        "trigger": "cron"
      }
    },
    {
      "name": "broken",
      "error": "bad toml"
    },
    {
      "name": "x",
      "schedule": {
        "source": "0 0 30 2 *",
        "cron": "0 0 30 2 *"
      },
      "paused": false,
      "next_run": null,
      "last_run": null,
      "history_error": "bad jsonl"
    }
  ]
}
"#
        );
        assert_eq!(to_json(&[]), "{\n  \"jobs\": []\n}\n");
    }
}
