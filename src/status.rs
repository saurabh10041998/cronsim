//! `cronsim status`: one job's details plus its most recent runs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use jiff::Zoned;
use jiff::tz::TimeZone;
use serde::Serialize;

use crate::format::{self, TIME_FORMAT};
use crate::history::{Outcome, RunRecord, Trigger};
use crate::job::{Command, JobSpec, OverlapPolicy, TtyMode};
use crate::lint;
use crate::schedule::Schedule;
use crate::store::{Store, StoreError};

pub const RECENT_RUNS: usize = 10;
pub const NEXT_RUNS: usize = 3;

pub struct Status {
    pub name: String,
    pub job: Result<Details, String>,
}

pub struct Details {
    pub spec: JobSpec,
    pub path: PathBuf,
    pub next_runs: Vec<Zoned>,
    pub history: Result<History, String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub struct History {
    /// Newest first.
    pub recent: Vec<RunRecord>,
    pub total: usize,
    pub last_ok: Option<RunRecord>,
    pub last_failure: Option<RunRecord>,
}

impl History {
    fn new(runs: Vec<RunRecord>) -> Self {
        let newest = || runs.iter().rev();
        Self {
            recent: newest().take(RECENT_RUNS).cloned().collect(),
            total: runs.len(),
            last_ok: newest()
                .find(|run| run.outcome == Outcome::Exited(0))
                .cloned(),
            last_failure: newest().find(|run| is_failure(&run.outcome)).cloned(),
        }
    }
}

fn is_failure(outcome: &Outcome) -> bool {
    !matches!(outcome, Outcome::Exited(0) | Outcome::Skipped(_))
}

impl Status {
    /// Problems reading the spec or the history.
    pub fn errors(&self) -> Vec<&str> {
        match &self.job {
            Err(e) => vec![e],
            Ok(details) => details
                .history
                .as_ref()
                .err()
                .map(String::as_str)
                .into_iter()
                .collect(),
        }
    }

    /// Schedule warnings and lint findings.
    pub fn warnings(&self) -> &[String] {
        self.job.as_ref().map_or(&[], |details| &details.warnings)
    }
}

pub fn status(store: &Store, name: &str, now: &Zoned) -> Result<Status, StoreError> {
    if !store.job_exists(name)? {
        return Err(StoreError::NotFound(name.to_string()));
    }
    Ok(build(store, name.to_string(), store.load_job(name), now))
}

/// Every job, sorted by name.
pub fn status_all(store: &Store, now: &Zoned) -> Result<Vec<Status>, StoreError> {
    Ok(store
        .list_jobs()?
        .into_iter()
        .map(|(name, spec)| build(store, name, spec, now))
        .collect())
}

fn build(store: &Store, name: String, spec: Result<JobSpec, StoreError>, now: &Zoned) -> Status {
    let job = spec.map_err(|e| e.to_string()).map(|spec| {
        let warnings = spec
            .schedule
            .warnings()
            .into_iter()
            .chain(
                lint::lint_command(&spec.command)
                    .into_iter()
                    .map(|finding| format!("{finding} (hint: {})", finding.hint)),
            )
            .collect();
        Details {
            path: store.job_path(&name),
            next_runs: if spec.paused {
                Vec::new()
            } else {
                spec.schedule.next_runs(now, NEXT_RUNS)
            },
            history: store
                .runs(&name)
                .map(History::new)
                .map_err(|e| e.to_string()),
            warnings,
            spec,
        }
    });
    Status { name, job }
}

pub fn render(statuses: &[Status], tz: &TimeZone, zone: &str) -> String {
    let blocks: Vec<String> = statuses.iter().map(|s| render_one(s, tz)).collect();
    format!("{}time zone: {zone}\n", blocks.join("\n"))
}

const INDENT: &str = "              ";

fn render_one(status: &Status, tz: &TimeZone) -> String {
    let mut out = format!("{}\n", status.name);
    let details = match &status.job {
        Ok(details) => details,
        Err(e) => {
            out.push_str(&format!("  error: {e}\n"));
            return out;
        }
    };
    let spec = &details.spec;
    let schedule = &spec.schedule;
    let mut field = |label: &str, value: String| {
        out.push_str(&format!("  {:<12}{value}\n", format!("{label}:")));
    };
    field(
        "schedule",
        match schedule.typed_form() {
            Some(typed) => format!("{schedule} (from {typed:?})"),
            None => schedule.to_string(),
        },
    );
    field("meaning", schedule.describe());
    field("command", format::command_line(&spec.command));
    field(
        "workdir",
        spec.workdir
            .as_ref()
            .map_or_else(|| "-".into(), |dir| dir.display().to_string()),
    );
    field("env", env_lines(&spec.env));
    field(
        "timeout",
        spec.timeout
            .map_or_else(|| "none".into(), |t| format::exact_duration(t.as_secs())),
    );
    field(
        "overlap",
        match spec.overlap {
            OverlapPolicy::Skip => "skip",
            OverlapPolicy::Wait => "wait",
        }
        .into(),
    );
    field(
        "tty",
        match spec.tty {
            TtyMode::NonInteractive => "non-interactive",
            TtyMode::Pty => "pty",
        }
        .into(),
    );
    field(
        "on failure",
        spec.on_failure
            .as_deref()
            .map_or_else(|| "-".into(), format::argv_line),
    );
    field("keep runs", spec.keep_runs.to_string());
    field(
        "state",
        if spec.paused { "paused" } else { "active" }.into(),
    );
    field("next runs", next_runs(&details.next_runs, spec.paused));
    field("spec", details.path.display().to_string());

    let history = match &details.history {
        Ok(history) => history,
        Err(_) => {
            field("runs", "history unreadable".into());
            return out;
        }
    };
    if history.total == 0 {
        field("runs", "never run".into());
        return out;
    }
    let when = |run: &Option<RunRecord>| {
        run.as_ref()
            .map_or_else(|| "never".into(), |run| format::local_time(run.started, tz))
    };
    field(
        "runs",
        format!(
            "{} kept; last ok {}; last failure {}",
            history.total,
            when(&history.last_ok),
            when(&history.last_failure)
        ),
    );

    out.push('\n');
    let mut rows = vec![["RUN ID", "STARTED", "DURATION", "RESULT"].map(String::from)];
    for run in &history.recent {
        rows.push([
            run.id.to_string(),
            format::local_time(run.started, tz),
            format::duration((run.finished - run.started).whole_seconds()),
            result(run),
        ]);
    }
    for line in format::table(&rows) {
        out.push_str(&format!("  {line}\n"));
    }
    let older = history.total - history.recent.len();
    if older > 0 {
        out.push_str(&format!(
            "  … {older} older run{}; cronsim logs {} --run <ID> shows the output of one\n",
            if older == 1 { "" } else { "s" },
            status.name
        ));
    }
    out
}

fn env_lines(env: &BTreeMap<String, String>) -> String {
    if env.is_empty() {
        return "-".into();
    }
    env.iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(&format!("\n{INDENT}"))
}

/// The first in full, then only the time while the date stays the same.
fn next_runs(runs: &[Zoned], paused: bool) -> String {
    let Some(first) = runs.first() else {
        return if paused { "paused" } else { "never" }.into();
    };
    std::iter::once(first.strftime(TIME_FORMAT).to_string())
        .chain(runs[1..].iter().map(|run| {
            if run.date() == first.date() {
                run.strftime("%H:%M").to_string()
            } else {
                run.strftime(TIME_FORMAT).to_string()
            }
        }))
        .collect::<Vec<_>>()
        .join(", ")
}

fn result(run: &RunRecord) -> String {
    let mut text = format::outcome_detail(&run.outcome);
    if run.trigger == Trigger::Manual {
        text.push_str(", manual");
    }
    text
}

#[derive(Serialize)]
struct JsonStatus<'a> {
    jobs: Vec<JsonJob<'a>>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum JsonJob<'a> {
    Job(Box<JsonDetails<'a>>),
    Broken { name: &'a str, error: &'a str },
}

#[derive(Serialize)]
struct JsonDetails<'a> {
    name: &'a str,
    spec: &'a Path,
    schedule: &'a Schedule,
    description: String,
    command: &'a Command,
    workdir: Option<&'a Path>,
    env: &'a BTreeMap<String, String>,
    timeout: Option<u64>,
    overlap: OverlapPolicy,
    tty: TtyMode,
    on_failure: Option<&'a [String]>,
    keep_runs: usize,
    paused: bool,
    next_runs: Vec<String>,
    warnings: &'a [String],
    runs_kept: Option<usize>,
    last_ok: Option<&'a RunRecord>,
    last_failure: Option<&'a RunRecord>,
    runs: &'a [RunRecord],
    #[serde(skip_serializing_if = "Option::is_none")]
    history_error: Option<&'a str>,
}

pub fn to_json(statuses: &[Status]) -> String {
    let jobs = statuses
        .iter()
        .map(|status| match &status.job {
            Err(error) => JsonJob::Broken {
                name: &status.name,
                error,
            },
            Ok(details) => {
                let spec = &details.spec;
                let history = details.history.as_ref().ok();
                JsonJob::Job(Box::new(JsonDetails {
                    name: &status.name,
                    spec: &details.path,
                    schedule: &spec.schedule,
                    description: spec.schedule.describe(),
                    command: &spec.command,
                    workdir: spec.workdir.as_deref(),
                    env: &spec.env,
                    timeout: spec.timeout.map(|t| t.as_secs()),
                    overlap: spec.overlap,
                    tty: spec.tty,
                    on_failure: spec.on_failure.as_deref(),
                    keep_runs: spec.keep_runs,
                    paused: spec.paused,
                    next_runs: details
                        .next_runs
                        .iter()
                        .map(|t| t.timestamp().to_string())
                        .collect(),
                    warnings: &details.warnings,
                    runs_kept: history.map(|h| h.total),
                    last_ok: history.and_then(|h| h.last_ok.as_ref()),
                    last_failure: history.and_then(|h| h.last_failure.as_ref()),
                    runs: history.map_or(&[], |h| &h.recent),
                    history_error: details.history.as_ref().err().map(String::as_str),
                }))
            }
        })
        .collect();
    let mut text =
        serde_json::to_string_pretty(&JsonStatus { jobs }).expect("job status serialises to JSON");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use jiff::Timestamp;
    use time::OffsetDateTime;

    use super::*;
    use crate::history::{RunId, SkipReason};
    use crate::store::tests::store;

    fn now() -> Zoned {
        "2026-10-26T10:07:00Z"
            .parse::<Timestamp>()
            .unwrap()
            .to_zoned(TimeZone::UTC)
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

    fn run(job: &str, minute: i64, secs: i64, outcome: Outcome, trigger: Trigger) -> RunRecord {
        let started = OffsetDateTime::from_unix_timestamp(1_792_980_000 + minute * 60).unwrap();
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

    fn cron(job: &str, minute: i64, outcome: Outcome) -> RunRecord {
        run(job, minute, 3, outcome, Trigger::Cron)
    }

    #[test]
    fn history_keeps_recent_runs_and_last_results() {
        let (_dir, store) = store();
        store.save_job(&spec("backup", "every 15m")).unwrap();
        let mut runs = vec![cron("backup", 0, Outcome::TimedOut)];
        runs.extend((1..11).map(|i| cron("backup", i, Outcome::Exited(0))));
        runs.push(cron("backup", 11, Outcome::Skipped(SkipReason::Overlap)));
        for record in &runs {
            store.append_run(record).unwrap();
        }

        let status = status(&store, "backup", &now()).unwrap();
        let details = status.job.unwrap();
        let history = details.history.unwrap();
        assert_eq!(history.total, 12);
        assert_eq!(history.recent.len(), RECENT_RUNS);
        assert_eq!(history.recent[0], runs[11]);
        assert_eq!(history.recent[9], runs[2]);
        assert_eq!(history.last_ok.as_ref(), Some(&runs[10]));
        assert_eq!(history.last_failure.as_ref(), Some(&runs[0]));
        assert_eq!(details.path, store.root().join("jobs/backup.toml"));
        let next: Vec<_> = details
            .next_runs
            .iter()
            .map(|t| t.timestamp().to_string())
            .collect();
        assert_eq!(
            next,
            [
                "2026-10-26T10:15:00Z",
                "2026-10-26T10:30:00Z",
                "2026-10-26T10:45:00Z"
            ]
        );
        assert!(details.warnings.is_empty());
    }

    #[test]
    fn no_history_paused_and_never_firing() {
        let (_dir, store) = store();
        let mut paused = spec("paused", "@hourly");
        paused.paused = true;
        store.save_job(&paused).unwrap();
        store.save_job(&spec("never", "0 0 30 2 *")).unwrap();
        store
            .append_run(&cron("paused", 0, Outcome::Exited(1)))
            .unwrap();

        let paused = status(&store, "paused", &now()).unwrap().job.unwrap();
        assert!(paused.next_runs.is_empty());
        assert_eq!(paused.history.unwrap().total, 1);

        let never = status(&store, "never", &now()).unwrap().job.unwrap();
        assert!(never.next_runs.is_empty());
        assert_eq!(
            never.history,
            Ok(History {
                recent: vec![],
                total: 0,
                last_ok: None,
                last_failure: None,
            })
        );
        assert_eq!(never.warnings.len(), 1, "{:?}", never.warnings);
    }

    #[test]
    fn missing_and_invalid_names_are_errors() {
        let (_dir, store) = store();
        assert!(matches!(
            status(&store, "nope", &now()),
            Err(StoreError::NotFound(name)) if name == "nope"
        ));
        assert!(matches!(
            status(&store, "../x", &now()),
            Err(StoreError::InvalidName(_))
        ));
    }

    #[test]
    fn unreadable_spec_and_history_are_reported() {
        let (_dir, store) = store();
        store.save_job(&spec("a", "@daily")).unwrap();
        store.save_job(&spec("c", "@daily")).unwrap();
        fs::write(store.root().join("jobs/b.toml"), "???").unwrap();
        fs::write(store.root().join("runs/c.jsonl"), "garbage\n").unwrap();

        let broken = status(&store, "b", &now()).unwrap();
        let [error] = broken.errors()[..] else {
            panic!("one error expected");
        };
        assert!(error.contains("b.toml"), "{error}");

        let all = status_all(&store, &now()).unwrap();
        let names: Vec<_> = all.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert!(all[0].errors().is_empty());
        let [error] = all[2].errors()[..] else {
            panic!("one error expected");
        };
        assert!(error.contains("c.jsonl:1:"), "{error}");
    }

    #[test]
    fn warnings_include_schedule_and_lint() {
        let (_dir, store) = store();
        let mut job = spec("j", "*/7 * * * *");
        job.command = Command::Exec {
            program: "sudo".into(),
            args: vec!["apt".into(), "update".into()],
        };
        store.save_job(&job).unwrap();
        let status = status(&store, "j", &now()).unwrap();
        let warnings = status.warnings();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("*/7"), "{}", warnings[0]);
        assert!(warnings[1].contains("sudo"), "{}", warnings[1]);
    }

    fn details(spec: JobSpec, next: &[&str], history: Result<History, String>) -> Details {
        Details {
            path: PathBuf::from(format!("/data/jobs/{}.toml", spec.name)),
            next_runs: next
                .iter()
                .map(|t| t.parse::<Timestamp>().unwrap().to_zoned(TimeZone::UTC))
                .collect(),
            history,
            warnings: vec![],
            spec,
        }
    }

    fn full_spec() -> JobSpec {
        let mut job = spec("backup", "every 15m");
        job.command = Command::Exec {
            program: "tar".into(),
            args: vec!["czf".into(), "/tmp/b.tgz".into(), "my docs".into()],
        };
        job.workdir = Some("/home/u".into());
        job.env = [("PATH", "/usr/bin:/bin"), ("TOKEN", "abc")]
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .into();
        job.timeout = Some(Duration::from_secs(3600));
        job.on_failure = Some(vec!["notify".into(), "backup failed".into()]);
        job
    }

    fn full_history() -> History {
        let runs = vec![
            run(
                "backup",
                0,
                0,
                Outcome::SpawnFailed("No such file".into()),
                Trigger::Manual,
            ),
            run("backup", 1, 0, Outcome::Signaled(9), Trigger::Cron),
            run("backup", 2, 0, Outcome::Signaled(64), Trigger::Cron),
            run("backup", 3, 3600, Outcome::TimedOut, Trigger::Cron),
            run("backup", 4, 3, Outcome::Exited(0), Trigger::Cron),
        ];
        let mut history = History::new(runs);
        history.total = 7;
        history
    }

    #[test]
    fn render_details_and_runs() {
        let status = Status {
            name: "backup".into(),
            job: Ok(details(
                full_spec(),
                &[
                    "2026-10-26T10:15:00Z",
                    "2026-10-26T23:45:00Z",
                    "2026-10-27T00:00:00Z",
                ],
                Ok(full_history()),
            )),
        };
        assert_eq!(
            render(&[status], &TimeZone::UTC, "UTC (system local)"),
            "\
backup
  schedule:   */15 * * * * (from \"every 15m\")
  meaning:    Every 15th minute
  command:    tar czf /tmp/b.tgz 'my docs'
  workdir:    /home/u
  env:        PATH=/usr/bin:/bin
              TOKEN=abc
  timeout:    1h
  overlap:    skip
  tty:        non-interactive
  on failure: notify 'backup failed'
  keep runs:  50
  state:      active
  next runs:  Mon 2026-10-26 10:15, 23:45, Tue 2026-10-27 00:00
  spec:       /data/jobs/backup.toml
  runs:       7 kept; last ok Mon 2026-10-26 02:04; last failure Mon 2026-10-26 02:03

  RUN ID                  STARTED                DURATION   RESULT
  20261026T020400Z-4242   Mon 2026-10-26 02:04   3s         ok
  20261026T020300Z-4242   Mon 2026-10-26 02:03   1h         timed out
  20261026T020200Z-4242   Mon 2026-10-26 02:02   0s         killed by signal 64
  20261026T020100Z-4242   Mon 2026-10-26 02:01   0s         killed by SIGKILL (signal 9)
  20261026T020000Z-4242   Mon 2026-10-26 02:00   0s         failed to start: No such file, manual
  … 2 older runs; cronsim logs backup --run <ID> shows the output of one
time zone: UTC (system local)
"
        );
    }

    #[test]
    fn render_short_cases() {
        let mut paused = spec("paused", "@daily");
        paused.paused = true;
        paused.timeout = None;
        let statuses = [
            Status {
                name: "broken".into(),
                job: Err("bad toml".into()),
            },
            Status {
                name: "paused".into(),
                job: Ok(details(paused, &[], Ok(History::new(vec![])))),
            },
            Status {
                name: "x".into(),
                job: Ok(details(spec("x", "0 0 30 2 *"), &[], Err("bad".into()))),
            },
        ];
        assert_eq!(
            render(&statuses, &TimeZone::UTC, "UTC (system local)"),
            "\
broken
  error: bad toml

paused
  schedule:   0 0 * * * (from \"@daily\")
  meaning:    At 00:00, every day
  command:    true
  workdir:    -
  env:        -
  timeout:    none
  overlap:    skip
  tty:        non-interactive
  on failure: -
  keep runs:  50
  state:      paused
  next runs:  paused
  spec:       /data/jobs/paused.toml
  runs:       never run

x
  schedule:   0 0 30 2 *
  meaning:    At 00:00, on day 30 of the month in February
  command:    true
  workdir:    -
  env:        -
  timeout:    none
  overlap:    skip
  tty:        non-interactive
  on failure: -
  keep runs:  50
  state:      active
  next runs:  never
  spec:       /data/jobs/x.toml
  runs:       history unreadable
time zone: UTC (system local)
"
        );
    }

    #[test]
    fn json_format() {
        let mut history = History::new(vec![cron("backup", 0, Outcome::Exited(2))]);
        history.recent.truncate(1);
        let statuses = [
            Status {
                name: "backup".into(),
                job: Ok(details(full_spec(), &["2026-10-26T10:15:00Z"], Ok(history))),
            },
            Status {
                name: "broken".into(),
                job: Err("bad toml".into()),
            },
            Status {
                name: "x".into(),
                job: Ok(details(spec("x", "@daily"), &[], Err("bad jsonl".into()))),
            },
        ];
        assert_eq!(
            to_json(&statuses),
            r#"{
  "jobs": [
    {
      "name": "backup",
      "spec": "/data/jobs/backup.toml",
      "schedule": {
        "source": "every 15m",
        "cron": "*/15 * * * *"
      },
      "description": "Every 15th minute",
      "command": {
        "kind": "exec",
        "program": "tar",
        "args": [
          "czf",
          "/tmp/b.tgz",
          "my docs"
        ]
      },
      "workdir": "/home/u",
      "env": {
        "PATH": "/usr/bin:/bin",
        "TOKEN": "abc"
      },
      "timeout": 3600,
      "overlap": "skip",
      "tty": "non-interactive",
      "on_failure": [
        "notify",
        "backup failed"
      ],
      "keep_runs": 50,
      "paused": false,
      "next_runs": [
        "2026-10-26T10:15:00Z"
      ],
      "warnings": [],
      "runs_kept": 1,
      "last_ok": null,
      "last_failure": {
        "id": "20261026T020000Z-4242",
        "job": "backup",
        "started": "2026-10-26T02:00:00Z",
        "finished": "2026-10-26T02:00:03Z",
        "outcome": {
          "exited": 2
        },
        "log": "logs/backup/20261026T020000Z-4242.log",
        "trigger": "cron"
      },
      "runs": [
        {
          "id": "20261026T020000Z-4242",
          "job": "backup",
          "started": "2026-10-26T02:00:00Z",
          "finished": "2026-10-26T02:00:03Z",
          "outcome": {
            "exited": 2
          },
          "log": "logs/backup/20261026T020000Z-4242.log",
          "trigger": "cron"
        }
      ]
    },
    {
      "name": "broken",
      "error": "bad toml"
    },
    {
      "name": "x",
      "spec": "/data/jobs/x.toml",
      "schedule": {
        "source": "@daily",
        "cron": "0 0 * * *"
      },
      "description": "At 00:00, every day",
      "command": {
        "kind": "exec",
        "program": "true",
        "args": []
      },
      "workdir": null,
      "env": {},
      "timeout": null,
      "overlap": "skip",
      "tty": "non-interactive",
      "on_failure": null,
      "keep_runs": 50,
      "paused": false,
      "next_runs": [],
      "warnings": [],
      "runs_kept": null,
      "last_ok": null,
      "last_failure": null,
      "runs": [],
      "history_error": "bad jsonl"
    }
  ]
}
"#
        );
        assert_eq!(to_json(&[]), "{\n  \"jobs\": []\n}\n");
    }
}
