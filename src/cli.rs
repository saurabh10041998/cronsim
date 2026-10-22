use std::path::PathBuf;

use clap::{ArgGroup, Args, Parser, Subcommand};

use crate::schedule::Schedule;

const MAX_JOB_NAME_LEN: usize = 64;

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
        run: Option<String>,
        /// Keep printing output as it is written
        #[arg(short, long)]
        follow: bool,
    },
    /// Run a job now
    Run {
        #[arg(value_parser = parse_job_name)]
        name: String,
        /// Reproduce cron's environment and conditions exactly
        #[arg(long)]
        as_cron: bool,
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
        /// Cron expression, e.g. "*/15 * * * *" or "@daily"
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
    /// Check the environment: crond, crontab access, TTY lint on jobs
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
    /// Cron expression, e.g. "*/15 * * * *" or "@daily"
    #[arg(short, long)]
    pub schedule: Schedule,
    /// Script to run
    #[arg(long, group = "target", value_name = "PATH")]
    pub script: Option<PathBuf>,
    /// Command and arguments to run, given after `--`
    #[arg(last = true, group = "target", value_name = "CMD")]
    pub cmd: Vec<String>,
}

/// Job names become file names and crontab tags, so keep them to a safe charset.
fn parse_job_name(s: &str) -> Result<String, String> {
    if s.is_empty() || s.len() > MAX_JOB_NAME_LEN {
        return Err(format!(
            "job name must be 1-{MAX_JOB_NAME_LEN} characters long"
        ));
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("job name may only contain A-Z, a-z, 0-9, '-' and '_'".to_string());
    }
    Ok(s.to_string())
}

#[cfg(test)]
mod tests {
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
            "*/15 * * * *",
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
        assert_eq!(add.cmd, ["tar", "-czf", "b.tgz"]);
        assert!(add.script.is_none());
    }

    #[test]
    fn add_with_script() {
        let cli = parse(&[
            "add",
            "report",
            "--schedule",
            "30 2 * * *",
            "--script",
            "./r.sh",
        ])
        .unwrap();
        let Command::Add(add) = cli.command else {
            panic!("expected add");
        };
        assert_eq!(add.script, Some(PathBuf::from("./r.sh")));
        assert!(add.cmd.is_empty());
    }

    #[test]
    fn add_requires_exactly_one_target() {
        let err = parse(&["add", "j", "-s", "@daily"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);

        let err = parse(&["add", "j", "-s", "@daily", "--script", "a.sh", "--", "ls"]).unwrap_err();
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
    fn job_name_validation() {
        for ok in ["a", "nightly-backup", "job_2", &"x".repeat(64)] {
            assert_eq!(parse_job_name(ok).as_deref(), Ok(ok));
        }
        for bad in ["", &"x".repeat(65), "a/b", "a.b", "a b", "jöb", ".."] {
            assert!(parse_job_name(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn invalid_schedule_is_rejected_by_cli() {
        for args in [
            &["next", "61 * * * *"][..],
            &["next", "every 15m"],
            &["add", "j", "-s", "@reboot", "--", "ls"],
        ] {
            let err = parse(args).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::ValueValidation, "{args:?}");
        }
    }

    #[test]
    fn invalid_name_is_rejected_by_cli() {
        let err = parse(&["run", "../etc"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ValueValidation);
    }
}
