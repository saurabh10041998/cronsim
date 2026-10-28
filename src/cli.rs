use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use clap::{ArgGroup, Args, Parser, Subcommand};

use crate::add;
use crate::history::RunId;
use crate::job::{self, OverlapPolicy};
use crate::schedule::Schedule;

#[derive(Debug, Parser)]
#[command(name = "cronsim", version, about = "Simplified cron scheduling")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Schedule a command or a script
    Add(AddArgs),
    /// List jobs with their schedule, next run and last result
    List {
        /// Print machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Show job details plus the last runs
    Status {
        #[arg(value_parser = parse_job_name)]
        name: Option<String>,
        /// Print machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Show (or follow) the output of a run
    Logs {
        #[arg(value_parser = parse_job_name)]
        name: String,
        /// Run id to show (defaults to the latest run)
        #[arg(long, value_name = "ID")]
        run: Option<RunId>,
        /// Keep printing output as it is written
        #[arg(short, long)]
        follow: bool,
    },
    /// Run a job now
    Run {
        #[arg(value_parser = parse_job_name)]
        name: String,
        /// Start from cron's minimal environment instead of yours
        #[arg(long)]
        as_cron: bool,
        /// Don't set TERM=dumb, NO_COLOR=1, PAGER=cat, … for the job
        #[arg(long)]
        no_noninteractive_env: bool,
    },
    /// Comment out the job's crontab line
    Pause {
        #[arg(value_parser = parse_job_name)]
        name: String,
    },
    /// Restore the job's crontab line
    Resume {
        #[arg(value_parser = parse_job_name)]
        name: String,
    },
    /// Delete the job and its crontab line
    Remove {
        #[arg(value_parser = parse_job_name)]
        name: String,
        /// Also delete the run history and logs
        #[arg(long)]
        purge: bool,
    },
    /// Show the next fire times of a schedule without scheduling anything
    Next {
        /// Cron expression, @alias, or e.g. "every 15m", "daily at 02:30", "mon,thu at 18:15"
        schedule: Schedule,
        /// Number of fire times to show
        #[arg(
            short = 'n',
            long = "count",
            default_value_t = 5,
            value_parser = clap::value_parser!(u16).range(1..)
        )]
        count: u16,
    },
    /// Check the environment: crond, crontab access, TTY and script lint on jobs
    Doctor,
    /// Called by cron
    #[command(hide = true)]
    Exec {
        #[arg(value_parser = parse_job_name)]
        name: String,
    },
}

#[derive(Debug, Args)]
#[command(group = ArgGroup::new("target").required(true).multiple(false))]
pub struct AddArgs {
    #[arg(value_parser = parse_job_name)]
    pub name: String,
    /// Cron expression, @alias, or e.g. "every 15m", "daily at 02:30", "mon,thu at 18:15"
    #[arg(short, long)]
    pub schedule: Schedule,
    /// Script to run (must be executable)
    #[arg(long, group = "target", value_name = "PATH", value_parser = parse_script)]
    pub script: Option<PathBuf>,
    /// Command and arguments to run, given after `--`
    #[arg(last = true, group = "target", value_name = "CMD")]
    pub cmd: Vec<String>,
    /// Kill the job after this long (e.g. 90s, 30m, 1h, 2d); 0 = no timeout
    #[arg(long, value_name = "DUR", default_value = "1h", value_parser = parse_duration)]
    pub timeout: Duration,
    /// What to do when the previous run is still going
    #[arg(long, value_name = "POLICY", default_value = "skip")]
    pub overlap: OverlapPolicy,
    /// Set a variable for the job (repeatable)
    #[arg(long = "env", value_name = "KEY=VAL", value_parser = parse_env)]
    pub env: Vec<(String, String)>,
    /// Copy these variables from the current shell (comma-separated; "" for none)
    #[arg(long, value_name = "NAMES", default_value = "PATH", value_parser = parse_env_names)]
    pub capture_env: EnvNames,
    /// Directory to run in [default: the home directory, as cron does].
    /// Relative paths in the command's arguments are resolved from here
    #[arg(long, value_name = "DIR", value_parser = parse_workdir)]
    pub workdir: Option<PathBuf>,
    /// Number of runs and logs to keep
    #[arg(long, value_name = "N", default_value_t = job::DEFAULT_KEEP_RUNS as u32,
          value_parser = clap::value_parser!(u32).range(1..))]
    pub keep_runs: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvNames(pub Vec<String>);

fn parse_job_name(s: &str) -> Result<String, String> {
    job::validate_name(s).map(|()| s.to_string())
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    if s == "0" {
        return Ok(Duration::ZERO);
    }
    let bad = || format!("expected 0 or a number with s, m, h or d (e.g. 90s, 30m, 1h), got '{s}'");
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    let (n, unit) = s.split_at(digits);
    let n: u64 = n.parse().map_err(|_| bad())?;
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return Err(bad()),
    };
    n.checked_mul(scale)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("'{s}' is too long"))
}

fn check_env_key(key: &str) -> Result<(), String> {
    let mut bytes = key.bytes();
    let valid = bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "'{key}' is not a valid variable name (letters, digits and '_', not starting with a digit)"
        ))
    }
}

fn parse_env(s: &str) -> Result<(String, String), String> {
    let (key, value) = s
        .split_once('=')
        .ok_or_else(|| format!("expected KEY=VAL, got '{s}'"))?;
    check_env_key(key)?;
    Ok((key.to_string(), value.to_string()))
}

fn parse_env_names(s: &str) -> Result<EnvNames, String> {
    if s.is_empty() {
        return Ok(EnvNames(Vec::new()));
    }
    s.split(',')
        .map(|name| check_env_key(name).map(|()| name.to_string()))
        .collect::<Result<_, _>>()
        .map(EnvNames)
}

fn parse_script(s: &str) -> Result<PathBuf, String> {
    let path = fs::canonicalize(s).map_err(|e| e.to_string())?;
    if !path.is_file() {
        return Err("not a regular file".to_string());
    }
    if !add::is_executable(&path) {
        return Err(format!("not executable; run 'chmod +x {s}'"));
    }
    Ok(path)
}

fn parse_workdir(s: &str) -> Result<PathBuf, String> {
    let path = fs::canonicalize(s).map_err(|e| e.to_string())?;
    if !path.is_dir() {
        return Err("not a directory".to_string());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use clap::CommandFactory;
    use clap::error::ErrorKind;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("cronsim").chain(args.iter().copied()))
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn add_with_command_keeps_dashed_args() {
        let cli = parse(&[
            "add",
            "backup",
            "-s",
            "every 15m",
            "--",
            "tar",
            "-czf",
            "b.tgz",
        ])
        .unwrap();
        let Command::Add(add) = cli.command else {
            panic!("expected add");
        };
        assert_eq!(add.name, "backup");
        assert_eq!(add.schedule.to_string(), "*/15 * * * *");
        assert_eq!(add.schedule.source(), "every 15m");
        assert_eq!(add.cmd, ["tar", "-czf", "b.tgz"]);
        assert!(add.script.is_none());
    }

    #[test]
    fn add_with_script() {
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("r.sh");
        fs::write(&script, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let link = dir.path().join("link.sh");
        std::os::unix::fs::symlink(&script, &link).unwrap();

        let cli = parse(&[
            "add",
            "report",
            "--schedule",
            "daily at 02:30",
            "--script",
            link.to_str().unwrap(),
        ])
        .unwrap();
        let Command::Add(add) = cli.command else {
            panic!("expected add");
        };
        assert_eq!(add.schedule.to_string(), "30 2 * * *");
        assert_eq!(add.script, Some(fs::canonicalize(&script).unwrap()));
        assert!(add.cmd.is_empty());

        fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
        let script = script.to_str().unwrap();
        for (path, message) in [
            (script, "not executable; run 'chmod +x "),
            (dir.path().to_str().unwrap(), "not a regular file"),
            ("/no/such/file", "No such file or directory"),
        ] {
            let err = parse(&["add", "j", "-s", "@daily", "--script", path]).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::ValueValidation, "{path}");
            assert!(err.to_string().contains(message), "{err}");
        }
    }

    #[test]
    fn add_option_defaults() {
        let Command::Add(add) = parse(&["add", "j", "-s", "@daily", "--", "ls"])
            .unwrap()
            .command
        else {
            panic!("expected add");
        };
        assert_eq!(add.timeout, Duration::from_secs(3600));
        assert_eq!(add.overlap, OverlapPolicy::Skip);
        assert_eq!(add.capture_env, EnvNames(vec!["PATH".into()]));
        assert!(add.env.is_empty());
        assert_eq!(add.workdir, None);
        assert_eq!(add.keep_runs, 50);
    }

    #[test]
    fn durations() {
        for (input, secs) in [
            ("0", 0),
            ("90s", 90),
            ("30m", 1800),
            ("1h", 3600),
            ("2d", 172800),
        ] {
            assert_eq!(
                parse_duration(input),
                Ok(Duration::from_secs(secs)),
                "{input}"
            );
        }
        for bad in ["", "5", "1h30m", "-5m", "5x", "h", "99999999999999999999d"] {
            assert!(parse_duration(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn env_values_and_names() {
        assert_eq!(parse_env("A=b"), Ok(("A".into(), "b".into())));
        assert_eq!(parse_env("_X1="), Ok(("_X1".into(), "".into())));
        assert_eq!(parse_env("A=b=c"), Ok(("A".into(), "b=c".into())));
        for bad in ["=x", "1A=x", "NOEQUALS", "A-B=x"] {
            assert!(parse_env(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            parse_env_names("PATH,HOME"),
            Ok(EnvNames(vec!["PATH".into(), "HOME".into()]))
        );
        assert_eq!(parse_env_names(""), Ok(EnvNames(vec![])));
        for bad in ["PATH,,HOME", "A-B", ","] {
            assert!(parse_env_names(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn add_rejects_bad_option_values() {
        for args in [
            &["--overlap", "never"][..],
            &["--keep-runs", "0"],
            &["--workdir", "/no/such/dir"],
            &["--workdir", "/etc/passwd"],
        ] {
            let argv = ["add", "j", "-s", "@daily"]
                .iter()
                .chain(args)
                .chain(&["--", "ls"])
                .copied()
                .collect::<Vec<_>>();
            assert!(parse(&argv).is_err(), "{args:?}");
        }
    }

    #[test]
    fn add_requires_exactly_one_target() {
        let err = parse(&["add", "j", "-s", "@daily"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);

        let err = parse(&[
            "add", "j", "-s", "@daily", "--script", "/bin/sh", "--", "ls",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn add_requires_schedule() {
        let err = parse(&["add", "j", "--", "ls"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn all_other_commands_parse() {
        for args in [
            &["list"][..],
            &["list", "--json"],
            &["status"],
            &["status", "job", "--json"],
            &["logs", "job"],
            &["logs", "job", "--run", "20261021T021500Z-42", "-f"],
            &["run", "job", "--as-cron"],
            &["run", "job", "--no-noninteractive-env"],
            &["pause", "job"],
            &["resume", "job"],
            &["remove", "job", "--purge"],
            &["next", "*/5 * * * *", "-n", "3"],
            &["doctor"],
            &["exec", "job"],
        ] {
            parse(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
    }

    #[test]
    fn next_count_defaults_to_five_and_rejects_zero() {
        let cli = parse(&["next", "@hourly"]).unwrap();
        assert!(matches!(cli.command, Command::Next { count: 5, .. }));

        let err = parse(&["next", "@hourly", "-n", "0"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn missing_name_and_unknown_command_fail() {
        assert_eq!(
            parse(&["pause"]).unwrap_err().kind(),
            ErrorKind::MissingRequiredArgument
        );
        assert_eq!(
            parse(&["frobnicate"]).unwrap_err().kind(),
            ErrorKind::InvalidSubcommand
        );
    }

    #[test]
    fn invalid_schedule_is_rejected_by_cli() {
        for args in [
            &["next", "61 * * * *"][..],
            &["next", "every 7m"],
            &["add", "j", "-s", "@reboot", "--", "ls"],
        ] {
            let err = parse(args).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::ValueValidation, "{args:?}");
        }
    }

    #[test]
    fn logs_run_id_is_validated() {
        let cli = parse(&["logs", "j", "--run", "20261021T021500Z-42"]).unwrap();
        let Command::Logs { run: Some(id), .. } = cli.command else {
            panic!("expected logs with a run id");
        };
        assert_eq!(id.pid(), 42);
        let err = parse(&["logs", "j", "--run", "garbage"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
    }

    #[test]
    fn invalid_name_is_rejected_by_cli() {
        let err = parse(&["run", "../etc"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
    }
}
