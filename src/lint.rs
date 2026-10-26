//! Heuristic lint for commands that break without a terminal (TTY.md §5).

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::job::Command;

const MAX_SCRIPT: u64 = 1 << 20;
const SHELLS: [&str; 5] = ["sh", "bash", "dash", "ash", "zsh"];
const WRAPPERS: [&str; 16] = [
    "exec", "env", "nohup", "time", "command", "nice", "if", "then", "else", "elif", "do", "while",
    "until", "!", "{", "}",
];
const RC_FILES: [&str; 4] = [".bashrc", ".bash_profile", ".profile", ".zshrc"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub line: Option<usize>,
    pub message: String,
    pub hint: &'static str,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(line) = self.line {
            write!(f, "line {line}: ")?;
        }
        f.write_str(&self.message)
    }
}

pub fn lint_command(command: &Command) -> Vec<Finding> {
    match command {
        Command::Script { path } => {
            read_script(path).map_or_else(Vec::new, |text| lint_script(&text))
        }
        Command::Exec { program, args } => {
            let is_shell = SHELLS.contains(&basename(program));
            match args.iter().position(|arg| arg == "-c") {
                Some(i) if is_shell => args.get(i + 1).map_or_else(Vec::new, |s| lint_script(s)),
                _ => {
                    let argv: Vec<String> = std::iter::once(program).chain(args).cloned().collect();
                    lint_argv(&argv)
                }
            }
        }
    }
}

fn read_script(path: &Path) -> Option<String> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_SCRIPT)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.contains(&0) {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

pub fn lint_argv(argv: &[String]) -> Vec<Finding> {
    let mut findings = Vec::new();
    if let Some((cmd, args)) = argv.split_first() {
        lint_simple(basename(cmd), args, false, None, &mut findings);
    }
    findings
}

pub fn lint_script(text: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (line, logical) in logical_lines(text) {
        for (words, piped) in simple_commands(&logical) {
            let Some(start) = command_start(&words) else {
                continue;
            };
            let before = findings.len();
            lint_simple(
                basename(&words[start]),
                &words[start + 1..],
                piped,
                Some(line),
                &mut findings,
            );
            let mut i = before;
            while i < findings.len() {
                if findings[..i].contains(&findings[i]) {
                    findings.remove(i);
                } else {
                    i += 1;
                }
            }
        }
    }
    findings
}

fn logical_lines(text: &str) -> Vec<(usize, String)> {
    let mut lines = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for (index, raw) in text.lines().enumerate() {
        let (start, mut text) = current.take().unwrap_or((index + 1, String::new()));
        match raw.strip_suffix('\\') {
            Some(rest) => {
                text.push_str(rest);
                current = Some((start, text));
            }
            None => {
                text.push_str(raw);
                lines.push((start, text));
            }
        }
    }
    lines.extend(current);
    lines
}

fn simple_commands(line: &str) -> Vec<(Vec<String>, bool)> {
    let mut commands = Vec::new();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut piped = false;
    let mut chars = line.chars().peekable();
    let mut end_command = |words: &mut Vec<String>, piped: bool| {
        if !words.is_empty() {
            commands.push((std::mem::take(words), piped));
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    word.push(c);
                }
            }
            '"' => {
                in_word = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => word.extend(chars.next()),
                        c => word.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.extend(chars.next());
            }
            '#' if !in_word => break,
            '&' if word.ends_with(['>', '<']) => word.push(c),
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
                end_command(&mut words, piped);
                piped = false;
            }
            ';' | '&' | '|' | '(' | ')' | '`' => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
                end_command(&mut words, piped);
                piped = c == '|' && chars.peek() != Some(&'|');
                if c == '|' && chars.peek() == Some(&'|') {
                    chars.next();
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    end_command(&mut words, piped);
    commands
}

fn command_start(words: &[String]) -> Option<usize> {
    let mut after_wrapper = false;
    words.iter().position(|word| {
        let skip = is_assignment(word)
            || WRAPPERS.contains(&word.as_str())
            || (after_wrapper
                && (word.starts_with('-') || word.bytes().all(|b| b.is_ascii_digit())));
        after_wrapper |= WRAPPERS.contains(&word.as_str());
        !skip
    })
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    })
}

fn basename(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

fn short_flags_contain(args: &[String], flag: char) -> bool {
    args.iter().any(|a| {
        a.strip_prefix('-').is_some_and(|group| {
            !group.is_empty()
                && group.bytes().all(|b| b.is_ascii_alphabetic())
                && group.contains(flag)
        })
    })
}

fn lint_simple(
    cmd: &str,
    args: &[String],
    piped: bool,
    line: Option<usize>,
    findings: &mut Vec<Finding>,
) {
    let mut add = |message: String, hint: &'static str| {
        findings.push(Finding {
            line,
            message,
            hint,
        })
    };
    match cmd {
        "sudo" => {
            let flags: Vec<String> = args
                .iter()
                .take_while(|a| a.starts_with('-'))
                .cloned()
                .collect();
            if !flags.iter().any(|a| a == "--non-interactive") && !short_flags_contain(&flags, 'n')
            {
                add(
                    "'sudo' without -n waits for a password under cron".into(),
                    "use 'sudo -n' with a NOPASSWD rule, or run the job as that user",
                );
            }
        }
        "docker" => {
            let sub = args
                .iter()
                .find(|a| !a.starts_with('-'))
                .map(String::as_str);
            if let Some(sub @ ("run" | "exec")) = sub
                && (args.iter().any(|a| a == "--tty" || a.starts_with("--tty="))
                    || short_flags_contain(args, 't'))
            {
                add(
                    format!(
                        "'docker {sub}' with -t fails under cron: the input device is not a TTY"
                    ),
                    "drop -t; keep -i only if you pipe input",
                );
            }
        }
        "ssh" => {
            if short_flags_contain(args, 't') {
                add(
                    "'ssh -t' asks for a terminal, which cron doesn't have".into(),
                    "drop -t",
                );
            }
            if !args
                .iter()
                .any(|a| a.to_ascii_lowercase().contains("batchmode=yes"))
            {
                add(
                    "'ssh' without BatchMode may wait for a password or host-key prompt".into(),
                    "add -o BatchMode=yes and use key-based auth",
                );
            }
        }
        "read" if !piped && !args.iter().any(|a| a.starts_with('<')) => add(
            "'read' gets no input under cron (stdin is empty)".into(),
            "pass the value as an argument or env var",
        ),
        "tput" | "clear" | "stty" => add(
            format!("'{cmd}' needs a terminal"),
            "guard with [ -t 1 ], or remove it",
        ),
        "less" | "more" | "vi" | "vim" | "nano" | "top" | "watch" => add(
            format!("'{cmd}' is interactive"),
            "not suitable for unattended runs; remove it",
        ),
        "source" | "." => {
            if let Some(file) = args.first()
                && RC_FILES.iter().any(|rc| basename(file) == *rc)
            {
                add(
                    format!("sourcing '{file}' may run prompts or stty"),
                    "source a minimal env file instead",
                );
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages(script: &str) -> Vec<String> {
        lint_script(script)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn rules_fire_on_problems() {
        for (script, expected) in [
            (
                "sudo apt update",
                "line 1: 'sudo' without -n waits for a password under cron",
            ),
            (
                "docker run -it alpine sh",
                "line 1: 'docker run' with -t fails under cron: the input device is not a TTY",
            ),
            (
                "docker exec --tty c sh",
                "line 1: 'docker exec' with -t fails under cron: the input device is not a TTY",
            ),
            (
                "ssh -o BatchMode=yes -t host",
                "line 1: 'ssh -t' asks for a terminal, which cron doesn't have",
            ),
            (
                "ssh host uptime",
                "line 1: 'ssh' without BatchMode may wait for a password or host-key prompt",
            ),
            (
                "read x",
                "line 1: 'read' gets no input under cron (stdin is empty)",
            ),
            ("tput cols", "line 1: 'tput' needs a terminal"),
            ("git log | less", "line 1: 'less' is interactive"),
            (
                ". ~/.bashrc",
                "line 1: sourcing '~/.bashrc' may run prompts or stty",
            ),
            (
                "source $HOME/.profile",
                "line 1: sourcing '$HOME/.profile' may run prompts or stty",
            ),
        ] {
            assert_eq!(messages(script), [expected], "{script}");
        }
    }

    #[test]
    fn fixes_are_quiet() {
        for script in [
            "sudo -n apt update",
            "sudo -nE true",
            "sudo --non-interactive true",
            "docker run -i alpine sh",
            "docker ps -t",
            "ssh -o BatchMode=yes host uptime",
            "ssh -oBatchMode=yes host",
            "cat file | read x",
            "read x < file",
            ". ./env.sh",
            "echo hi",
        ] {
            assert_eq!(messages(script), Vec::<String>::new(), "{script}");
        }
    }

    #[test]
    fn tokenizer() {
        for (script, expected) in [
            ("echo hi # sudo x", vec![]),
            ("echo \"sudo x\" 'tput'", vec![]),
            (
                "cd /x && sudo y",
                vec!["line 1: 'sudo' without -n waits for a password under cron"],
            ),
            ("echo $(tput cols)", vec!["line 1: 'tput' needs a terminal"]),
            ("echo `tput cols`", vec!["line 1: 'tput' needs a terminal"]),
            (
                "VAR=1 sudo x",
                vec!["line 1: 'sudo' without -n waits for a password under cron"],
            ),
            (
                "env FOO=1 nice -n 5 /usr/bin/sudo x",
                vec!["line 1: 'sudo' without -n waits for a password under cron"],
            ),
            (
                "if [ -t 1 ]; then tput cols; fi",
                vec!["line 1: 'tput' needs a terminal"],
            ),
            ("echo x 2>&1 | tee log", vec![]),
            (
                "#!/bin/sh\necho start \\\n  && sudo x\ntput cols",
                vec![
                    "line 2: 'sudo' without -n waits for a password under cron",
                    "line 4: 'tput' needs a terminal",
                ],
            ),
            (
                "read a; read b",
                vec!["line 1: 'read' gets no input under cron (stdin is empty)"],
            ),
        ] {
            assert_eq!(messages(script), expected, "{script}");
        }
    }

    #[test]
    fn commands() {
        let exec = |program: &str, args: &[&str]| Command::Exec {
            program: program.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
        };
        let lint = |cmd: &Command| -> Vec<String> {
            lint_command(cmd).iter().map(ToString::to_string).collect()
        };
        assert_eq!(
            lint(&exec("sh", &["-c", "echo; sudo x"])),
            ["line 1: 'sudo' without -n waits for a password under cron"]
        );
        assert_eq!(
            lint(&exec("/usr/bin/sudo", &["apt", "update"])),
            ["'sudo' without -n waits for a password under cron"]
        );
        assert_eq!(lint(&exec("tar", &["-c", "sudo"])), Vec::<String>::new());

        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("job.sh");
        std::fs::write(&script, "#!/bin/sh\ntput cols\n").unwrap();
        assert_eq!(
            lint(&Command::Script {
                path: script.clone()
            }),
            ["line 2: 'tput' needs a terminal"]
        );
        std::fs::write(&script, b"\x7fELF\0sudo").unwrap();
        assert_eq!(
            lint(&Command::Script { path: script }),
            Vec::<String>::new()
        );
        let missing = Command::Script {
            path: dir.path().join("missing"),
        };
        assert_eq!(lint(&missing), Vec::<String>::new());
    }
}
