//! Run history: one JSON `RunRecord` per line in `runs/<name>.jsonl`.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

use crate::store::{self, JobLock, Store, StoreError};

/// `20261021T021500Z-<pid>`: the UTC start second plus the pid, so ids sort
/// in time order as plain strings.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RunId(String);

impl RunId {
    pub fn new(started: OffsetDateTime, pid: u32) -> Self {
        let t = started.to_offset(UtcOffset::UTC);
        Self(format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}Z-{pid}",
            t.year(),
            u8::from(t.month()),
            t.day(),
            t.hour(),
            t.minute(),
            t.second()
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The pid of the `cronsim` process that ran it.
    pub fn pid(&self) -> u32 {
        parse(&self.0).expect("run id was validated").1
    }

    pub fn started(&self) -> OffsetDateTime {
        parse(&self.0).expect("run id was validated").0
    }
}

impl FromStr for RunId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (started, pid) = parse(s)?;
        let id = Self::new(started, pid);
        if id.0 != s {
            return Err(invalid_id(s));
        }
        Ok(id)
    }
}

fn invalid_id(s: &str) -> String {
    format!("invalid run id '{s}'; expected e.g. 20261021T021500Z-4242")
}

fn parse(s: &str) -> Result<(OffsetDateTime, u32), String> {
    let invalid = || invalid_id(s);
    let (stamp, pid) = s.split_once("Z-").ok_or_else(invalid)?;
    if pid.is_empty() || pid.len() > 10 || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let (date, time) = stamp.split_once('T').ok_or_else(invalid)?;
    if date.len() != 8 || time.len() != 6 {
        return Err(invalid());
    }
    let num = |part: &str| part.parse::<u16>().map_err(|_| invalid());
    let month = Month::try_from(num(&date[4..6])? as u8).map_err(|_| invalid())?;
    let date = Date::from_calendar_date(num(&date[..4])?.into(), month, num(&date[6..])? as u8)
        .map_err(|_| invalid())?;
    let time = Time::from_hms(
        num(&time[..2])? as u8,
        num(&time[2..4])? as u8,
        num(&time[4..])? as u8,
    )
    .map_err(|_| invalid())?;
    let started = PrimitiveDateTime::new(date, time).assume_utc();
    Ok((started, pid.parse().map_err(|_| invalid())?))
}

impl TryFrom<String> for RunId {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<RunId> for String {
    fn from(id: RunId) -> Self {
        id.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: RunId,
    pub job: String,
    #[serde(with = "time::serde::rfc3339")]
    pub started: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub finished: OffsetDateTime,
    pub outcome: Outcome,
    /// Relative to the data root.
    pub log: PathBuf,
    pub trigger: Trigger,
}

impl RunRecord {
    pub fn new(
        job: &str,
        id: RunId,
        started: OffsetDateTime,
        finished: OffsetDateTime,
        outcome: Outcome,
        trigger: Trigger,
    ) -> Result<Self, StoreError> {
        Ok(Self {
            log: store::log_file(job, &id)?,
            id,
            job: job.to_string(),
            started,
            finished,
            outcome,
            trigger,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Exited(i32),
    Signaled(i32),
    TimedOut,
    Skipped(SkipReason),
    SpawnFailed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    Overlap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    Cron,
    Manual,
}

impl Store {
    /// One `O_APPEND` write per record. This doesn't take the job lock, so a
    /// run skipped for overlap can still be recorded.
    pub fn append_run(&self, record: &RunRecord) -> Result<(), StoreError> {
        store::check_name(&record.job)?;
        let path = self.runs_path(&record.job);
        let result = (|| {
            let mut file = store::new_file()
                .read(true)
                .append(true)
                .create(true)
                .open(&path)?;
            let mut line = serde_json::to_string(record).expect("run record serialises");
            line.push('\n');
            if !ends_with_newline(&mut file)? {
                line.insert(0, '\n');
            }
            file.write_all(line.as_bytes())?;
            file.sync_data()
        })();
        result.map_err(|e| store::io_error(&path, e))
    }

    /// Oldest first. A missing file means no runs yet.
    pub fn runs(&self, name: &str) -> Result<Vec<RunRecord>, StoreError> {
        store::check_name(name)?;
        let path = self.runs_path(name);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(store::io_error(&path, e)),
        };
        let mut records = Vec::new();
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(line) {
                Ok(record) => records.push(record),
                // A crash in the middle of an append leaves a truncated line.
                Err(e) if e.is_eof() => {}
                Err(source) => {
                    return Err(StoreError::Json {
                        path,
                        line: index + 1,
                        source,
                    });
                }
            }
        }
        Ok(records)
    }

    pub fn last_run(&self, name: &str) -> Result<Option<RunRecord>, StoreError> {
        Ok(self.runs(name)?.pop())
    }

    /// Keeps the newest `keep` records and deletes every log they don't
    /// reference, including logs of runs that never wrote a record.
    pub fn prune(&self, lock: &JobLock, keep: usize) -> Result<(), StoreError> {
        let name = lock.name();
        let mut records = self.runs(name)?;
        if records.len() > keep {
            records.drain(..records.len() - keep);
            let mut text = String::new();
            for record in &records {
                text.push_str(&serde_json::to_string(record).expect("run record serialises"));
                text.push('\n');
            }
            store::write_atomic(&self.runs_path(name), text.as_bytes())?;
        }

        let kept: HashSet<String> = records.iter().map(|r| format!("{}.log", r.id)).collect();
        let dir = self.logs_dir(name);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(store::io_error(&dir, e)),
        };
        for entry in entries {
            let entry = entry.map_err(|e| store::io_error(&dir, e))?;
            let file_name = entry.file_name();
            let is_log = file_name.to_str().is_some_and(|n| n.ends_with(".log"));
            if is_log && !kept.contains(file_name.to_str().unwrap_or_default()) {
                let path = entry.path();
                store::ignore_missing(fs::remove_file(&path))
                    .map_err(|e| store::io_error(&path, e))?;
            }
        }
        Ok(())
    }
}

fn ends_with_newline(file: &mut fs::File) -> io::Result<bool> {
    if file.metadata()?.len() == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0];
    file.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;

    use time::Duration;

    use super::*;
    use crate::store::tests::store;

    fn start() -> OffsetDateTime {
        let date = Date::from_calendar_date(2026, Month::October, 21).unwrap();
        let time = Time::from_hms_milli(2, 15, 0, 120).unwrap();
        PrimitiveDateTime::new(date, time).assume_utc()
    }

    fn record(job: &str, offset_minutes: i64, outcome: Outcome) -> RunRecord {
        let started = start() + Duration::minutes(offset_minutes);
        RunRecord::new(
            job,
            RunId::new(started, 4242),
            started,
            started + Duration::seconds(3),
            outcome,
            Trigger::Cron,
        )
        .unwrap()
    }

    #[test]
    fn run_id_format_and_order() {
        let id = RunId::new(start(), 4242);
        assert_eq!(id.as_str(), "20261021T021500Z-4242");
        let ist = start().to_offset(UtcOffset::from_hms(5, 30, 0).unwrap());
        assert_eq!(RunId::new(ist, 4242), id);
        let later = RunId::new(start() + Duration::seconds(1), 1);
        assert!(id < later);
        assert_eq!(id.to_string().parse::<RunId>(), Ok(id.clone()));
        assert_eq!(id.pid(), 4242);
        assert_eq!(id.started(), start().replace_millisecond(0).unwrap());
        assert_eq!(id.started().offset(), UtcOffset::UTC);
    }

    #[test]
    fn run_id_rejects_bad_input() {
        for bad in [
            "",
            "../x",
            "20261021T021500Z",
            "20261021T021500Z-",
            "20261021T021500Z-12a",
            "20261021T021500Z-../../x",
            "2026102T021500Z-1",
            "20261321T021500Z-1",
            "20261021T246000Z-1",
            "20260230T021500Z-1",
            "20261021t021500Z-1",
            "20261021T021500Z-01",
            "+2026102T021500Z-1",
        ] {
            assert!(bad.parse::<RunId>().is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn json_format() {
        let lines: Vec<String> = [
            Outcome::Exited(0),
            Outcome::Signaled(9),
            Outcome::TimedOut,
            Outcome::Skipped(SkipReason::Overlap),
            Outcome::SpawnFailed("not found".into()),
        ]
        .into_iter()
        .map(|outcome| serde_json::to_string(&record("backup", 0, outcome)).unwrap())
        .collect();
        assert_eq!(
            lines[0],
            r#"{"id":"20261021T021500Z-4242","job":"backup","started":"2026-10-21T02:15:00.12Z","finished":"2026-10-21T02:15:03.12Z","outcome":{"exited":0},"log":"logs/backup/20261021T021500Z-4242.log","trigger":"cron"}"#
        );
        let outcomes: Vec<&str> = lines
            .iter()
            .map(|l| {
                l.split(r#""outcome":"#)
                    .nth(1)
                    .unwrap()
                    .split(r#","log""#)
                    .next()
                    .unwrap()
            })
            .collect();
        assert_eq!(
            outcomes,
            [
                r#"{"exited":0}"#,
                r#"{"signaled":9}"#,
                r#""timed_out""#,
                r#"{"skipped":"overlap"}"#,
                r#"{"spawn_failed":"not found"}"#,
            ]
        );
    }

    #[test]
    fn append_and_read_back() {
        let (_dir, store) = store();
        assert!(store.runs("j").unwrap().is_empty());
        assert_eq!(store.last_run("j").unwrap(), None);

        let records: Vec<_> = (0..3)
            .map(|i| record("j", i, Outcome::Exited(i as i32)))
            .collect();
        for r in &records {
            store.append_run(r).unwrap();
        }
        assert_eq!(store.runs("j").unwrap(), records);
        assert_eq!(store.last_run("j").unwrap().as_ref(), records.last());
    }

    #[test]
    fn truncated_line_is_skipped() {
        let (_dir, store) = store();
        let first = record("j", 0, Outcome::TimedOut);
        store.append_run(&first).unwrap();
        let path = store.runs_path("j");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"id":"2026"#).unwrap();
        assert_eq!(store.runs("j").unwrap(), std::slice::from_ref(&first));

        let second = record("j", 1, Outcome::Exited(0));
        store.append_run(&second).unwrap();
        assert_eq!(store.runs("j").unwrap(), [first, second]);
    }

    #[test]
    fn corrupt_line_is_an_error_with_its_line_number() {
        let (_dir, store) = store();
        store
            .append_run(&record("j", 0, Outcome::TimedOut))
            .unwrap();
        let path = store.runs_path("j");
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("garbage\n");
        fs::write(&path, text).unwrap();
        let err = store.runs("j").unwrap_err();
        assert!(matches!(err, StoreError::Json { line: 2, .. }));
        assert!(err.to_string().contains("j.jsonl:2:"), "{err}");
    }

    #[test]
    fn prune_keeps_newest_records_and_their_logs() {
        let (_dir, store) = store();
        for i in 0..55 {
            let r = record("j", i, Outcome::Exited(0));
            store.create_log("j", &r.id).unwrap();
            store.append_run(&r).unwrap();
        }
        let orphan: RunId = "20200101T000000Z-1".parse().unwrap();
        store.create_log("j", &orphan).unwrap();
        fs::write(store.logs_dir("j").join("notes.txt"), "").unwrap();

        let lock = store.lock("j").unwrap();
        store.prune(&lock, 50).unwrap();

        let runs = store.runs("j").unwrap();
        assert_eq!(runs.len(), 50);
        assert_eq!(runs[0], record("j", 5, Outcome::Exited(0)));
        let mut logs: Vec<String> = fs::read_dir(store.logs_dir("j"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(".log"))
            .collect();
        logs.sort();
        let expected: Vec<String> = runs.iter().map(|r| format!("{}.log", r.id)).collect();
        assert_eq!(logs, expected);
        assert!(store.logs_dir("j").join("notes.txt").exists());

        store.prune(&lock, 100).unwrap();
        assert_eq!(store.runs("j").unwrap(), runs);
    }

    #[test]
    fn prune_without_history_is_a_no_op() {
        let (_dir, store) = store();
        let lock = store.lock("j").unwrap();
        store.prune(&lock, 50).unwrap();
        assert!(!store.runs_path("j").exists());
    }
}
