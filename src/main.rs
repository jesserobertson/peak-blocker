//! A Claude Code `PreToolUse` hook that pauses tool calls during Anthropic's weekday
//! peak window (Mon-Fri 05:00-11:00 US Pacific, about 01:00-07:00 NZDT Tue-Sat),
//! when session limits drain faster.
//!
//! Inside the window every tool call is denied except git and gh commands and a
//! few coordination tools, so running agents commit, push and stop, and nothing
//! new starts. The Pacific time comes from the IANA timezone database bundled by
//! `jiff`, so daylight saving follows the database, not rules written here. It
//! replaces `reference/peak-hours-guard.sh`, without starting a subprocess.
//!
//! Override: create `~/.claude/allow-peak-hours` to disable the guard until the
//! file is removed. Testing: `PEAK_HOURS_NOW_EPOCH=<unix seconds>` pretends it
//! is that moment.

use std::io::Read;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use jiff::Timestamp;
use serde_json::Value;

const PEAK_ZONE: &str = "America/Los_Angeles";

const DENY_REASON: &str = "Peak-hour pause (Mon-Fri 05:00-11:00 US Pacific, about 01:00-07:00 NZDT): Claude limits drain faster now. Commit and push your work with git, append your state to your report file, then stop and wait; do not retry other tools. Override: touch ~/.claude/allow-peak-hours";

/// Tools that coordinate running agents and never start new work.
const ALWAYS_ALLOWED: &[&str] = &[
    "SendMessage",
    "ListAgents",
    "TaskStop",
    "ReadNotifications",
    "CronList",
    "CronCreate",
    "CronDelete",
    "ScheduleWakeup",
    "ToolSearch",
    "TaskOutput",
];

/// Programs that mean a build or a test run, which git and gh commands must not run.
const BUILD_WORDS: &[&str] = &[
    "cargo", "rustc", "pixi", "mpirun", "pytest", "maturin", "nextest", "claude",
];

/// Whether `now` (Unix seconds) is inside Mon-Fri 05:00-11:00 US Pacific.
///
/// If the time cannot be converted the guard lets the call through: a broken
/// guard must not stop all work.
fn in_peak_window(now: i64) -> bool {
    let Ok(local) = Timestamp::from_second(now).and_then(|instant| instant.in_tz(PEAK_ZONE)) else {
        return false;
    };
    let weekday = local.weekday().to_monday_one_offset();
    weekday <= 5 && (5..11).contains(&local.hour())
}

/// The text of a `$(...)` command substitution starting at `chars[start]` (the `$`),
/// parentheses included, and the index just after it.
fn command_substitution(chars: &[char], start: usize) -> (String, usize) {
    let mut depth = 0;
    let mut end = start;
    while end < chars.len() {
        match chars[end] {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end += 1;
                    break;
                }
            }
            _ => {}
        }
        end += 1;
    }
    (chars[start..end].iter().collect(), end)
}

/// The command substitutions (`$(...)` and backticks) in `text`, which still run
/// when they sit inside double quotes or an unquoted heredoc.
fn substitutions_in(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut kept = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 2;
        } else if chars[i] == '$' && chars.get(i + 1) == Some(&'(') {
            let (found, next) = command_substitution(&chars, i);
            kept.push(' ');
            kept.push_str(&found);
            i = next;
        } else if chars[i] == '`' {
            let close = chars[i + 1..].iter().position(|c| *c == '`');
            let end = close.map_or(chars.len(), |p| i + 1 + p + 1);
            kept.push(' ');
            kept.extend(&chars[i..end]);
            i = end;
        } else {
            i += 1;
        }
    }
    kept
}

/// A heredoc whose body starts after the next newline.
struct Heredoc {
    delimiter: String,
    /// A quoted delimiter means the body is literal text.
    quoted: bool,
    /// `<<-` allows tabs before the closing delimiter.
    strip_tabs: bool,
}

/// `command` with the text of quoted strings and heredoc bodies removed, so that a
/// commit message that mentions `cargo` is not mistaken for running it. Anything that
/// would run stays: command substitutions inside double quotes and unquoted heredocs.
fn without_quoted_text(command: &str) -> String {
    let chars: Vec<char> = command.chars().collect();
    let mut out = String::new();
    let mut pending: Vec<Heredoc> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '\'' => {
                let close = chars[i + 1..].iter().position(|c| *c == '\'');
                i = close.map_or(chars.len(), |p| i + 1 + p + 1);
                out.push(' ');
            }
            '"' => {
                let mut j = i + 1;
                while j < chars.len() && chars[j] != '"' {
                    j += if chars[j] == '\\' { 2 } else { 1 };
                }
                let inside: String = chars[i + 1..j.min(chars.len())].iter().collect();
                out.push(' ');
                out.push_str(&substitutions_in(&inside));
                i = j + 1;
            }
            '<' if chars.get(i + 1) == Some(&'<') && chars.get(i + 2) != Some(&'<') => {
                let mut j = i + 2;
                let strip_tabs = chars.get(j) == Some(&'-');
                if strip_tabs {
                    j += 1;
                }
                while chars.get(j).is_some_and(|c| *c == ' ' || *c == '\t') {
                    j += 1;
                }
                let mut delimiter = String::new();
                let mut quoted = false;
                while let Some(&c) = chars.get(j) {
                    if c == '\'' || c == '"' {
                        quoted = true;
                    } else if c.is_whitespace() || ";&|<>()".contains(c) {
                        break;
                    } else {
                        delimiter.push(c);
                    }
                    j += 1;
                }
                pending.push(Heredoc {
                    delimiter,
                    quoted,
                    strip_tabs,
                });
                out.push_str("<<");
                i = j;
            }
            '\n' => {
                out.push('\n');
                i += 1;
                for heredoc in pending.drain(..) {
                    let mut body = String::new();
                    while i < chars.len() {
                        let line_end = chars[i..].iter().position(|c| *c == '\n');
                        let end = line_end.map_or(chars.len(), |p| i + p);
                        let line: String = chars[i..end].iter().collect();
                        i = (end + 1).min(chars.len());
                        let candidate = line.trim_end_matches('\r');
                        let candidate = if heredoc.strip_tabs {
                            candidate.trim_start_matches('\t')
                        } else {
                            candidate
                        };
                        if candidate == heredoc.delimiter {
                            break;
                        }
                        body.push_str(&line);
                        body.push('\n');
                    }
                    if !heredoc.quoted {
                        out.push_str(&substitutions_in(&body));
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// `command` without a leading `cd <dir> &&` or `cd <dir> ;`. A command without
/// that prefix is returned untouched.
fn without_leading_cd(command: &str) -> &str {
    let Some(rest) = command.strip_prefix("cd ") else {
        return command;
    };
    let Some(split) = rest.find(['&', ';', '|']) else {
        return command;
    };
    if split == 0 {
        return command;
    }
    let after = &rest[split..];
    let tail = if let Some(tail) = after.strip_prefix(';') {
        tail
    } else if let Some(tail) = after.strip_prefix("&&") {
        tail
    } else {
        return command;
    };
    tail.trim_start()
}

/// Whether `command` names a build or test program as a whole word, where a word
/// is delimited by anything other than a lowercase letter or underscore.
fn mentions_build_program(command: &str) -> bool {
    let bytes = command.as_bytes();
    let is_word_byte = |b: u8| b.is_ascii_lowercase() || b == b'_';
    BUILD_WORDS.iter().any(|word| {
        command.match_indices(word).any(|(at, _)| {
            let before_ok = at == 0 || !is_word_byte(bytes[at - 1]);
            let after = at + word.len();
            let after_ok = after >= bytes.len() || !is_word_byte(bytes[after]);
            before_ok && after_ok
        })
    })
}

/// Whether a shell command is a git or gh command that runs no build or test program.
/// This is a cost guard for agents, not a sandbox.
fn is_git_command(command: &str) -> bool {
    let code = without_quoted_text(command);
    let code = without_leading_cd(code.trim_start()).trim_start();
    let is_git = code == "git"
        || code == "gh"
        || code.starts_with("git ")
        || code.starts_with("gh ")
        || code.starts_with("git\n")
        || code.starts_with("gh\n");
    is_git && !mentions_build_program(code)
}

/// Whether the tool call (the hook's JSON input) is allowed during the peak window.
/// Input that is not valid JSON names no tool and is not allowed.
fn allowed_in_window(input: &str) -> bool {
    let Ok(call) = serde_json::from_str::<Value>(input) else {
        return false;
    };
    let tool = call["tool_name"].as_str().unwrap_or_default();
    if ALWAYS_ALLOWED.contains(&tool) {
        return true;
    }
    if tool == "Bash" || tool == "PowerShell" {
        return call["tool_input"]["command"]
            .as_str()
            .is_some_and(is_git_command);
    }
    false
}

fn home_dir() -> Option<PathBuf> {
    // USERPROFILE first: a hook started directly by Claude Code on Windows may have
    // no HOME, or a Unix-style one that Windows paths cannot resolve.
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn main() {
    // The override file disables the guard.
    if home_dir().is_some_and(|home| home.join(".claude").join("allow-peak-hours").exists()) {
        return;
    }

    let now = std::env::var("PEAK_HOURS_NOW_EPOCH")
        .ok()
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs() as i64)
        });

    // Outside the window everything is allowed, and the input is never read.
    if !in_peak_window(now) {
        return;
    }

    let mut raw = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut raw);
    let input = String::from_utf8_lossy(&raw);
    if allowed_in_window(&input) {
        return;
    }

    println!(
        "{{\"hookSpecificOutput\":{{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"{DENY_REASON}\"}}}}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// "<start> <end>" lines, in Unix seconds with the end excluded, for each stretch of
    /// hours inside the window from 2024 to 2030, written by Python's `zoneinfo` from
    /// the IANA database (`scripts/gen_window_intervals.py`).
    const ZONEINFO_WINDOWS: &str = include_str!("../tests/pacific_window_intervals.txt");

    /// 2024-01-01T00:00:00Z and 2031-01-01T00:00:00Z.
    const FIRST_SECOND: i64 = 1_704_067_200;
    const END_SECOND: i64 = 1_924_992_000;

    fn call(tool: &str, command: &str) -> String {
        json!({"tool_name": tool, "tool_input": {"command": command}}).to_string()
    }

    fn allowed(command: &str) -> bool {
        allowed_in_window(&call("Bash", command))
    }

    #[test]
    fn the_window_agrees_with_python_zoneinfo_for_every_hour_from_2024_to_2030() {
        let windows: Vec<(i64, i64)> = ZONEINFO_WINDOWS
            .lines()
            .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
            .map(|line| {
                let (start, end) = line.split_once(' ').expect("two fields per line");
                (
                    start.parse().expect("a start"),
                    end.parse().expect("an end"),
                )
            })
            .collect();
        assert!(
            windows.len() > 1_500,
            "only {} windows were read",
            windows.len()
        );
        let expected = |second: i64| {
            let after = windows.partition_point(|&(_, end)| end <= second);
            after < windows.len() && windows[after].0 <= second
        };
        // The first and the last second of every hour: the window only changes on the
        // hour, including across the clock changes.
        for hour_start in (FIRST_SECOND..END_SECOND).step_by(3600) {
            for second in [hour_start, hour_start + 3599] {
                assert_eq!(
                    in_peak_window(second),
                    expected(second),
                    "second {second} disagrees with zoneinfo"
                );
            }
        }
    }

    #[test]
    fn git_and_gh_pass_but_builds_and_other_tools_do_not() {
        assert!(allowed("git status"));
        assert!(allowed("cd /tmp && git log"));
        assert!(allowed("gh pr view 5"));
        assert!(allowed("git add -A && git commit -m x && git push"));
        assert!(!allowed("git log && cargo build"));
        assert!(!allowed("cargo test"));
        assert!(!allowed("ls"));
        assert!(allowed_in_window(&call("ToolSearch", "")));
        assert!(!allowed_in_window(&call("Edit", "")));
        assert!(!allowed_in_window("not json"));
        assert!(!allowed_in_window(""));
    }

    #[test]
    fn commits_with_messages_pass_whatever_the_message_says() {
        assert!(allowed(r#"git commit -m "fix the cargo notes""#));
        assert!(allowed(
            "git commit -m 'Co-Authored-By: Claude <noreply@anthropic.com>'"
        ));
        assert!(allowed(
            "git commit -F - <<'EOF'\nUse cargo and claude here; && rm -rf\n\nCo-Authored-By: Claude\nEOF"
        ));
        assert!(allowed(
            "git commit -F - <<EOF\nplain text with pytest\nEOF"
        ));
        assert!(allowed("git commit -F - <<-EOF\n\tindented body\n\tEOF"));
        assert!(allowed("  git status"));
        assert!(allowed("  cd /tmp && git status"));
        assert!(allowed(r#"cd "/my dir" && git status"#));
        assert!(allowed("git commit -m \"a\" && git push"));
    }

    #[test]
    fn a_build_that_would_run_is_denied_wherever_it_hides() {
        assert!(!allowed("git log $(cargo build)"));
        assert!(!allowed(r#"git log "$(cargo build)""#));
        assert!(!allowed("git log `cargo build`"));
        assert!(!allowed(r#"git log "`cargo build`""#));
        assert!(!allowed("git commit -F - <<EOF\n$(cargo build)\nEOF"));
        assert!(!allowed("git commit -m 'x' && cargo test"));
        assert!(!allowed(
            "git commit -F - <<'EOF'\nmessage\nEOF\ncargo test"
        ));
        // Text after a quoted heredoc's closing line is code again.
        assert!(!allowed("git status\ncargo build"));
    }

    #[test]
    fn no_short_run_of_quotes_heredocs_and_substitutions_makes_the_reader_panic() {
        // The binary aborts on a panic, so unterminated quotes, a trailing backslash and
        // half a heredoc must all be read without indexing past the end.
        const TOKENS: &[&str] = &[
            "'", "\"", "\\", "<<", "<<-", "$(", ")", "`", "\n", "git ", "EOF", " ",
        ];
        fn visit(prefix: &mut String, depth: usize) {
            let _ = is_git_command(prefix);
            if depth == 0 {
                return;
            }
            for token in TOKENS {
                let length = prefix.len();
                prefix.push_str(token);
                visit(prefix, depth - 1);
                prefix.truncate(length);
            }
        }
        visit(&mut String::new(), 5);
    }

    #[test]
    fn a_program_name_inside_a_longer_word_is_not_a_build() {
        assert!(allowed("git log --grep=cargos"));
        assert!(allowed("git log my_cargo"));
    }
}
