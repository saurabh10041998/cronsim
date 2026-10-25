use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::schedule::Schedule;

pub const MAX_NAME_LEN: usize = 64;
pub const FORMAT_VERSION: u32 = 1;
pub const DEFAULT_KEEP_RUNS: usize = 50;
/// The `PATH` cron gives a job when it sets none.
pub const CRON_PATH: &str = "/usr/bin:/bin";

/// Job names become file names and crontab tags, so keep them to a safe charset.
pub fn validate_name(s: &str) -> Result<(), String> {
    if s.is_empty() || s.len() > MAX_NAME_LEN {
        return Err(format!("job name must be 1-{MAX_NAME_LEN} characters long"));
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("job name may only contain A-Z, a-z, 0-9, '-' and '_'".to_string());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSpec {
    #[serde(default = "format_version")]
    pub version: u32,
    pub name: String,
    pub schedule: Schedule,
    pub command: Command,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "secs")]
    pub timeout: Option<Duration>,
    #[serde(default)]
    pub overlap: OverlapPolicy,
    #[serde(default)]
    pub tty: TtyMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_failure: Option<Vec<String>>,
    #[serde(
        default = "default_keep_runs",
        skip_serializing_if = "is_default_keep_runs"
    )]
    pub keep_runs: usize,
    #[serde(default)]
    pub paused: bool,
}

impl JobSpec {
    pub fn new(name: String, schedule: Schedule, command: Command) -> Self {
        Self {
            version: FORMAT_VERSION,
            name,
            schedule,
            command,
            workdir: None,
            env: BTreeMap::new(),
            timeout: None,
            overlap: OverlapPolicy::default(),
            tty: TtyMode::default(),
            on_failure: None,
            keep_runs: DEFAULT_KEEP_RUNS,
            paused: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Command {
    Exec { program: String, args: Vec<String> },
    Script { path: PathBuf },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum OverlapPolicy {
    #[default]
    Skip,
    Wait,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TtyMode {
    #[default]
    NonInteractive,
    Pty,
}

fn format_version() -> u32 {
    FORMAT_VERSION
}

fn default_keep_runs() -> usize {
    DEFAULT_KEEP_RUNS
}

fn is_default_keep_runs(keep: &usize) -> bool {
    *keep == DEFAULT_KEEP_RUNS
}

/// Timeouts are whole seconds on disk; `0` means none.
mod secs {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(duration) => s.serialize_u64(duration.as_secs()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        Ok(Option::<u64>::deserialize(d)?
            .filter(|&secs| secs > 0)
            .map(Duration::from_secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backup() -> JobSpec {
        let mut spec = JobSpec::new(
            "backup".into(),
            "every 15m".parse().unwrap(),
            Command::Exec {
                program: "tar".into(),
                args: vec!["-czf".into(), "/tmp/b.tgz".into(), "/srv".into()],
            },
        );
        spec.workdir = Some("/srv".into());
        spec.env.insert("PATH".into(), "/usr/bin:/bin".into());
        spec.timeout = Some(Duration::from_secs(3600));
        spec
    }

    #[test]
    fn job_name_validation() {
        for ok in ["a", "nightly-backup", "job_2", &"x".repeat(64)] {
            assert_eq!(validate_name(ok), Ok(()));
        }
        for bad in ["", &"x".repeat(65), "a/b", "a.b", "a b", "jöb", ".."] {
            assert!(validate_name(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn toml_format() {
        assert_eq!(
            toml::to_string(&backup()).unwrap(),
            r#"version = 1
name = "backup"
workdir = "/srv"
timeout = 3600
overlap = "skip"
tty = "non-interactive"
paused = false

[schedule]
source = "every 15m"
cron = "*/15 * * * *"

[command]
kind = "exec"
program = "tar"
args = ["-czf", "/tmp/b.tgz", "/srv"]

[env]
PATH = "/usr/bin:/bin"
"#
        );
    }

    #[test]
    fn round_trips() {
        let mut full = backup();
        full.overlap = OverlapPolicy::Wait;
        full.tty = TtyMode::Pty;
        full.on_failure = Some(vec!["notify".into(), "failed".into()]);
        full.keep_runs = 10;
        full.paused = true;
        let minimal = JobSpec::new(
            "report".into(),
            "@daily".parse().unwrap(),
            Command::Script {
                path: "/home/u/r.sh".into(),
            },
        );
        for spec in [backup(), full, minimal] {
            let text = toml::to_string(&spec).unwrap();
            assert_eq!(toml::from_str::<JobSpec>(&text).unwrap(), spec, "{text}");
        }
    }

    #[test]
    fn minimal_file_uses_defaults() {
        let spec: JobSpec = toml::from_str(
            r#"
            name = "j"
            timeout = 0
            schedule = { source = "@hourly", cron = "0 * * * *" }
            command = { kind = "script", path = "/a.sh" }
            "#,
        )
        .unwrap();
        assert_eq!(spec.version, FORMAT_VERSION);
        assert_eq!(spec.schedule.source(), "@hourly");
        assert_eq!(spec.schedule.to_string(), "0 * * * *");
        assert_eq!(spec.timeout, None);
        assert_eq!(spec.keep_runs, DEFAULT_KEEP_RUNS);
        assert_eq!(spec.overlap, OverlapPolicy::Skip);
        assert_eq!(spec.tty, TtyMode::NonInteractive);
        assert!(!spec.paused);
    }

    #[test]
    fn cron_is_authoritative_and_validated() {
        let spec: JobSpec = toml::from_str(
            r#"
            name = "j"
            schedule = { source = "every 15m", cron = "*/30 * * * *" }
            command = { kind = "script", path = "/a.sh" }
            "#,
        )
        .unwrap();
        assert_eq!(spec.schedule.to_string(), "*/30 * * * *");

        let err = toml::from_str::<JobSpec>(
            r#"
            name = "j"
            schedule = { source = "x", cron = "61 * * * *" }
            command = { kind = "script", path = "/a.sh" }
            "#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("minute: 61 is out of range 0-59"),
            "{err}"
        );
    }
}
