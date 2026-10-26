//! The non-interactive profile and cron's base environment (TTY.md §3–§4).

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

use crate::job::CRON_PATH;

pub const PROFILE: [(&str, &str); 7] = [
    ("TERM", "dumb"),
    ("NO_COLOR", "1"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("SYSTEMD_PAGER", ""),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("DEBIAN_FRONTEND", "noninteractive"),
];

pub fn build_env(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    job: &BTreeMap<String, String>,
    as_cron: bool,
    noninteractive: bool,
) -> BTreeMap<OsString, OsString> {
    let inherited: BTreeMap<OsString, OsString> = inherited.into_iter().collect();
    let mut env = if as_cron {
        let get = |key: &str| inherited.get(OsStr::new(key)).cloned();
        let mut base = BTreeMap::new();
        if let Some(home) = get("HOME") {
            base.insert("HOME".into(), home);
        }
        if let Some(user) = get("LOGNAME").or_else(|| get("USER")) {
            base.insert("LOGNAME".into(), user);
        }
        base.insert("SHELL".into(), "/bin/sh".into());
        base.insert("PATH".into(), CRON_PATH.into());
        base
    } else {
        inherited
    };
    env.extend(job.iter().map(|(k, v)| (k.into(), v.into())));
    if noninteractive {
        for (key, value) in PROFILE {
            if !job.contains_key(key) {
                env.insert(key.into(), value.into());
            }
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs.iter().map(|(k, v)| (k.into(), v.into())).collect()
    }

    fn job(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn get<'a>(env: &'a BTreeMap<OsString, OsString>, key: &str) -> Option<&'a str> {
        env.get(OsStr::new(key)).map(|v| v.to_str().unwrap())
    }

    const SHELL: &[(&str, &str)] = &[
        ("FOO", "1"),
        ("TERM", "xterm"),
        ("HOME", "/home/u"),
        ("USER", "u"),
        ("PATH", "/home/u/bin:/usr/bin"),
    ];

    #[test]
    fn plain_keeps_our_env_and_applies_profile() {
        let env = build_env(vars(SHELL), &job(&[]), false, true);
        assert_eq!(get(&env, "FOO"), Some("1"));
        assert_eq!(get(&env, "PATH"), Some("/home/u/bin:/usr/bin"));
        for (key, value) in PROFILE {
            assert_eq!(get(&env, key), Some(value), "{key}");
        }
        assert_eq!(env.len(), SHELL.len() + PROFILE.len() - 1);
    }

    #[test]
    fn job_env_wins_over_profile_and_base() {
        let env = build_env(
            vars(SHELL),
            &job(&[("TERM", "xterm-256color"), ("PATH", "/opt/bin")]),
            false,
            true,
        );
        assert_eq!(get(&env, "TERM"), Some("xterm-256color"));
        assert_eq!(get(&env, "PATH"), Some("/opt/bin"));
        assert_eq!(get(&env, "NO_COLOR"), Some("1"));
    }

    #[test]
    fn as_cron_starts_from_cron_env() {
        let env = build_env(vars(SHELL), &job(&[("A", "b")]), true, false);
        let all: Vec<_> = env
            .iter()
            .map(|(k, v)| (k.to_str().unwrap(), v.to_str().unwrap()))
            .collect();
        assert_eq!(
            all,
            [
                ("A", "b"),
                ("HOME", "/home/u"),
                ("LOGNAME", "u"),
                ("PATH", "/usr/bin:/bin"),
                ("SHELL", "/bin/sh"),
            ]
        );

        let env = build_env(
            vars(&[("LOGNAME", "l"), ("USER", "u")]),
            &job(&[("PATH", "/x")]),
            true,
            true,
        );
        assert_eq!(get(&env, "LOGNAME"), Some("l"));
        assert_eq!(get(&env, "PATH"), Some("/x"));
        assert_eq!(get(&env, "HOME"), None);
        assert_eq!(get(&env, "TERM"), Some("dumb"));
    }

    #[test]
    fn profile_can_be_turned_off() {
        let env = build_env(vars(SHELL), &job(&[]), false, false);
        assert_eq!(get(&env, "TERM"), Some("xterm"));
        assert_eq!(get(&env, "NO_COLOR"), None);
    }
}
