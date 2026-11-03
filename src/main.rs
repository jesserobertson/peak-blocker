//! A Claude Code `PreToolUse` hook that pauses tool calls during Anthropic's weekday
//! peak window (Mon-Fri 05:00-11:00 US Pacific), when session limits drain faster.
//!
//! Inside the window every tool call is denied except git and gh commands and a
//! few coordination tools, so running agents commit, push and stop, and nothing
//! new starts. The Pacific time comes from the IANA timezone database bundled by
//! `jiff`, so daylight saving follows the database, not rules written here. It
//! starts no subprocess.
//!
//! Override: `touch ~/.claude/allow-peak-hours` lets calls through for the peak
//! window it was touched in, or the next one if touched outside a window, and the
//! guard re-arms at the window after that. Testing: `PEAK_HOURS_NOW_EPOCH=<unix
//! seconds>` pretends it is that moment.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use jiff::Timestamp;
use serde_json::{json, Value};

const PEAK_ZONE: &str = "America/Los_Angeles";
const NZ_ZONE: &str = "Pacific/Auckland";

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

/// The tools that write a file, allowed only inside the report directory.
const WRITE_TOOLS: &[&str] = &["Write", "Edit", "MultiEdit"];

/// Programs that mean a build or a test run.
const BUILD_PROGRAMS: &[&str] = &[
    "cargo", "rustc", "pixi", "mpirun", "pytest", "maturin", "nextest", "claude",
];

/// Shells, interpreters and wrappers, which can run a build out of sight
/// (`bash -c 'cargo test'`, `env cargo build`).
const HIDING_PROGRAMS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "fish",
    "python",
    "python3",
    "py",
    "node",
    "npx",
    "bunx",
    "pwsh",
    "powershell",
    "cmd",
    "eval",
    "source",
    "env",
    "time",
    "nice",
    "nohup",
    "exec",
    "command",
    "sudo",
    "timeout",
    "stdbuf",
    "xargs",
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

/// The end of the first peak window that finishes after `after` (Unix seconds): 11:00
/// Pacific on the first weekday whose 11:00 is still ahead.
fn next_window_end(after: i64) -> Option<i64> {
    let local = Timestamp::from_second(after).ok()?.in_tz(PEAK_ZONE).ok()?;
    let mut date = local.date();
    for _ in 0..8 {
        if date.weekday().to_monday_one_offset() <= 5 {
            let end = date
                .at(11, 0, 0, 0)
                .in_tz(PEAK_ZONE)
                .ok()?
                .timestamp()
                .as_second();
            if end > after {
                return Some(end);
            }
        }
        date = date.tomorrow().ok()?;
    }
    None
}

/// Whether an override file last modified at `modified` still applies at `now`: it
/// covers the peak window it was touched in, or the next one, and no later.
fn override_applies(modified: i64, now: i64) -> bool {
    next_window_end(modified).is_none_or(|end| now < end)
}

/// The peak window as New Zealand clock times on the Pacific date of `now`, such as
/// `02:00-08:00 NZDT`.
fn window_in_new_zealand(now: i64) -> Option<String> {
    let pacific = Timestamp::from_second(now).ok()?.in_tz(PEAK_ZONE).ok()?;
    let date = pacific.date();
    let at = |hour: i8| {
        date.at(hour, 0, 0, 0)
            .in_tz(PEAK_ZONE)
            .ok()?
            .in_tz(NZ_ZONE)
            .ok()
    };
    let (start, end) = (at(5)?, at(11)?);
    let name = if start.offset().seconds() >= 13 * 3600 {
        "NZDT"
    } else {
        "NZST"
    };
    Some(format!(
        "{:02}:00-{:02}:00 {name}",
        start.hour(),
        end.hour()
    ))
}

/// What the denied agent is told. `report_dir` is where it may write its state.
fn deny_reason(now: i64, report_dir: Option<&Path>) -> String {
    let local = window_in_new_zealand(now)
        .map(|range| format!(", today {range}"))
        .unwrap_or_default();
    let report = report_dir
        .map(|dir| {
            format!(
                "write your state to a file under {} (Write and Edit are allowed there and nowhere else)",
                dir.display()
            )
        })
        .unwrap_or_else(|| "note your state in your final message".to_string());
    format!(
        "Peak-hour pause (Mon-Fri 05:00-11:00 US Pacific{local}): Claude limits drain faster now. Commit and push your work with git, {report}, then stop and wait; do not retry other tools. Override for this window: touch ~/.claude/allow-peak-hours"
    )
}

/// The directory where an agent may write its state during the window: the value of
/// `PEAK_BLOCKER_REPORT_DIR`, else `~/.claude/peak-reports`.
fn report_dir() -> Option<PathBuf> {
    std::env::var_os("PEAK_BLOCKER_REPORT_DIR")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".claude").join("peak-reports")))
}

/// The parts of `path` with `.` and `..` resolved, lower-cased where the file system
/// ignores case. `None` if a `..` climbs above the root.
fn path_parts(path: &str) -> Option<Vec<String>> {
    let mut parts: Vec<String> = Vec::new();
    for part in path.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other if cfg!(windows) => parts.push(other.to_lowercase()),
            other => parts.push(other.to_string()),
        }
    }
    Some(parts)
}

/// Whether `path` is an absolute path to something inside `dir`. The comparison is on
/// whole path parts, so `peak-reports-other` is not inside `peak-reports`.
fn is_inside(path: &str, dir: &Path) -> bool {
    let absolute = path.starts_with(['/', '\\']) || path.as_bytes().get(1) == Some(&b':');
    let (Some(path), Some(dir)) = (path_parts(path), path_parts(&dir.to_string_lossy())) else {
        return false;
    };
    absolute && !dir.is_empty() && path.len() > dir.len() && path[..dir.len()] == dir[..]
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

/// Whether `word` is a shell variable assignment such as `GH_TOKEN=abc`.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// The program a simple command runs: its first word that is not a variable
/// assignment, without any directory or `.exe`, in lower case.
fn program_of(segment: &str) -> Option<String> {
    let word = segment
        .split_whitespace()
        .find(|word| !is_assignment(word))?;
    let name = word.rsplit(['/', '\\']).next().unwrap_or(word);
    Some(
        name.strip_suffix(".exe")
            .unwrap_or(name)
            .to_ascii_lowercase(),
    )
}

/// The simple commands of `code`, split at `;`, `&`, `|`, newlines, parentheses,
/// braces and backticks (so also at `&&`, `||` and `$(`).
fn simple_commands(code: &str) -> impl Iterator<Item = &str> {
    code.split([';', '&', '|', '\n', '(', ')', '`', '{', '}'])
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
}

/// Whether a shell command is a git or gh command that runs no build or test program.
///
/// Leading `cd`, `export` and `NAME=value` setup is skipped. The first program after
/// that must be `git` or `gh`, and no later command may be a build program or a shell,
/// interpreter or wrapper that could hide one. Quoted text is not code, so a commit
/// message may say anything. This is a cost guard for agents, not a sandbox.
fn is_git_command(command: &str) -> bool {
    let code = without_quoted_text(command);
    let mut started = false;
    for segment in simple_commands(&code) {
        let Some(program) = program_of(segment) else {
            continue; // only variable assignments
        };
        if program == "cd" || program == "export" {
            continue;
        }
        if BUILD_PROGRAMS.contains(&program.as_str()) || HIDING_PROGRAMS.contains(&program.as_str())
        {
            return false;
        }
        if !started {
            if program != "git" && program != "gh" {
                return false;
            }
            started = true;
        }
    }
    started
}

/// Whether the tool call (the hook's JSON input) is allowed during the peak window.
/// Input that is not valid JSON names no tool and is not allowed. `report_dir` is the
/// only place the file-writing tools may write.
fn allowed_in_window(input: &str, report_dir: Option<&Path>) -> bool {
    let Ok(call) = serde_json::from_str::<Value>(input) else {
        return false;
    };
    let tool = call["tool_name"].as_str().unwrap_or_default();
    if ALWAYS_ALLOWED.contains(&tool) {
        return true;
    }
    if WRITE_TOOLS.contains(&tool) {
        return report_dir
            .zip(call["tool_input"]["file_path"].as_str())
            .is_some_and(|(dir, path)| is_inside(path, dir));
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

/// Whether `~/.claude/allow-peak-hours` exists and still applies at `now`. A file whose
/// age cannot be read is taken to apply: a broken guard must not stop all work.
fn override_file_applies(now: i64) -> bool {
    let Some(path) = home_dir().map(|home| home.join(".claude").join("allow-peak-hours")) else {
        return false;
    };
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_secs() as i64);
    modified.is_none_or(|modified| override_applies(modified, now))
}

fn main() {
    let now = std::env::var("PEAK_HOURS_NOW_EPOCH")
        .ok()
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs() as i64)
        });

    // Outside the window everything is allowed, and the input is never read.
    if !in_peak_window(now) || override_file_applies(now) {
        return;
    }

    let mut raw = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut raw);
    let input = String::from_utf8_lossy(&raw);
    let report_dir = report_dir();
    if allowed_in_window(&input, report_dir.as_deref()) {
        return;
    }

    println!(
        "{}",
        json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": deny_reason(now, report_dir.as_deref()),
            }
        })
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
        allowed_in_window(&call("Bash", command), None)
    }

    fn second(utc: &str) -> i64 {
        utc.parse::<Timestamp>().expect("a timestamp").as_second()
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
    fn the_window_ends_are_found_from_zoneinfo_windows_too() {
        // Every window end from the zoneinfo data is what next_window_end finds from any
        // second inside that window.
        for line in ZONEINFO_WINDOWS
            .lines()
            .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        {
            let (start, end) = line.split_once(' ').expect("two fields per line");
            let (start, end): (i64, i64) = (start.parse().unwrap(), end.parse().unwrap());
            assert_eq!(next_window_end(start), Some(end), "from the start {start}");
            assert_eq!(next_window_end(end - 1), Some(end), "from the last second");
        }
    }

    #[test]
    fn an_override_covers_its_own_window_or_the_next_and_then_re_arms() {
        // Wednesday 2026-11-04: the window is 13:00-19:00 UTC (05:00-11:00 PST).
        let during = second("2026-11-04T15:00:00Z");
        let window_end = second("2026-11-04T19:00:00Z");
        // Touched during a window: applies for the rest of it, not after.
        assert!(override_applies(during, during + 60));
        assert!(override_applies(during, window_end - 1));
        assert!(!override_applies(during, window_end));
        // The next day's window is guarded again.
        assert!(!override_applies(during, second("2026-11-05T15:00:00Z")));
        // Touched the evening before: covers the whole of the next window.
        let evening = second("2026-11-04T06:00:00Z"); // Tuesday 22:00 PST
        assert!(override_applies(evening, second("2026-11-04T13:00:00Z")));
        assert!(override_applies(evening, window_end - 1));
        assert!(!override_applies(evening, window_end));
        // Touched on Friday evening: covers Monday's window, not Tuesday's.
        let friday = second("2026-11-07T03:00:00Z"); // Friday 19:00 PST
        assert!(override_applies(friday, second("2026-11-09T18:59:59Z")));
        assert!(!override_applies(friday, second("2026-11-10T15:00:00Z")));
        // A forgotten file from last week no longer applies.
        assert!(!override_applies(second("2026-10-28T15:00:00Z"), during));
    }

    #[test]
    fn the_message_gives_the_new_zealand_hours_for_the_time_of_year() {
        // US standard time with NZ daylight time, US and NZ daylight time, US daylight
        // time with NZ standard time.
        let cases = [
            ("2026-11-04T16:00:00Z", "02:00-08:00 NZDT"),
            ("2026-10-14T16:00:00Z", "01:00-07:00 NZDT"),
            ("2026-07-08T16:00:00Z", "00:00-06:00 NZST"),
        ];
        for (when, range) in cases {
            assert_eq!(window_in_new_zealand(second(when)).as_deref(), Some(range));
            assert!(deny_reason(second(when), None).contains(range), "{when}");
        }
    }

    #[test]
    fn git_and_gh_pass_but_builds_and_other_tools_do_not() {
        assert!(allowed("git status"));
        assert!(allowed("cd /tmp && git log"));
        assert!(allowed("gh pr view 5"));
        assert!(allowed("git add -A && git commit -m x && git push"));
        assert!(allowed("git status | head"));
        assert!(!allowed("git log && cargo build"));
        assert!(!allowed("cargo test"));
        assert!(!allowed("ls"));
        assert!(allowed_in_window(&call("ToolSearch", ""), None));
        assert!(!allowed_in_window(&call("Edit", ""), None));
        assert!(!allowed_in_window("not json", None));
        assert!(!allowed_in_window("", None));
    }

    #[cfg(windows)]
    const REPORT_DIR: &str = r"C:\Users\me\.claude\peak-reports";
    #[cfg(not(windows))]
    const REPORT_DIR: &str = "/home/me/.claude/peak-reports";

    fn write_call(tool: &str, path: &str) -> String {
        json!({"tool_name": tool, "tool_input": {"file_path": path}}).to_string()
    }

    /// Whether `tool` may write `path` with the report directory in place.
    fn may_write(tool: &str, path: &str) -> bool {
        allowed_in_window(&write_call(tool, path), Some(Path::new(REPORT_DIR)))
    }

    #[test]
    fn the_file_tools_may_write_only_inside_the_report_directory() {
        for tool in ["Write", "Edit", "MultiEdit"] {
            assert!(may_write(tool, &format!("{REPORT_DIR}/state.md")), "{tool}");
            assert!(
                may_write(tool, &format!("{REPORT_DIR}/agent/state.md")),
                "{tool}"
            );
            assert!(
                may_write(tool, &format!("{REPORT_DIR}/./a/../state.md")),
                "{tool}"
            );
            // The directory itself is not a file inside it.
            assert!(!may_write(tool, REPORT_DIR), "{tool}");
            // A sibling that merely starts with the same letters is not inside.
            assert!(
                !may_write(tool, &format!("{REPORT_DIR}-other/state.md")),
                "{tool}"
            );
            // `..` cannot climb out of it.
            assert!(
                !may_write(tool, &format!("{REPORT_DIR}/../../elsewhere.md")),
                "{tool}"
            );
            assert!(
                !may_write(tool, &format!("{REPORT_DIR}/../state.md")),
                "{tool}"
            );
            // Only an absolute path counts.
            assert!(!may_write(tool, ".claude/peak-reports/state.md"), "{tool}");
            assert!(
                !may_write(tool, REPORT_DIR.trim_start_matches(['/', '\\'])),
                "{tool}"
            );
            assert!(!may_write(tool, "src/main.rs"), "{tool}");
            assert!(!may_write(tool, ""), "{tool}");
        }
        // No other tool writes, and Bash stays git-only.
        assert!(!may_write(
            "NotebookEdit",
            &format!("{REPORT_DIR}/state.md")
        ));
        assert!(!allowed(&format!("echo done >> {REPORT_DIR}/state.md")));
        // Without a report directory nothing may be written.
        let call = write_call("Write", &format!("{REPORT_DIR}/state.md"));
        assert!(!allowed_in_window(&call, None));
        assert!(!allowed_in_window(
            &json!({"tool_name": "Write"}).to_string(),
            Some(Path::new(REPORT_DIR))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_are_compared_without_regard_to_case_or_separator() {
        assert!(may_write(
            "Write",
            r"C:\USERS\ME\.CLAUDE\PEAK-REPORTS\State.md"
        ));
        assert!(may_write(
            "Write",
            "c:/users/me/.claude/peak-reports/state.md"
        ));
        assert!(!may_write(
            "Write",
            r"D:\Users\me\.claude\peak-reports\state.md"
        ));
    }

    #[test]
    fn the_message_names_the_report_directory() {
        let now = second("2026-11-04T16:00:00Z");
        let with = deny_reason(now, Some(Path::new(REPORT_DIR)));
        assert!(
            with.contains(REPORT_DIR) && with.contains("nowhere else"),
            "{with}"
        );
        let without = deny_reason(now, None);
        assert!(
            !without.contains("peak-reports") && without.contains("final message"),
            "{without}"
        );
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
    fn a_program_name_in_a_path_branch_or_option_is_not_a_build() {
        assert!(allowed("git add .claude/settings.json"));
        assert!(allowed("git add .cargo/config.toml"));
        assert!(allowed("git checkout -b feature/pytest-fixes"));
        assert!(allowed("git log --author=claude"));
        assert!(allowed("git log --grep=cargos"));
        assert!(allowed("git log my_cargo"));
        assert!(allowed("gh pr create --title cargo --body-file -"));
    }

    #[test]
    fn setup_before_git_is_skipped() {
        assert!(allowed("GH_TOKEN=x gh issue edit 5"));
        assert!(allowed("export GH_TOKEN=x; gh issue edit 5"));
        assert!(allowed(
            "GH_TOKEN=$(gh auth token --user jesserobertson) gh issue edit 5"
        ));
        assert!(allowed("cd a && cd b && git status"));
        assert!(allowed("git\tstatus"));
        assert!(allowed("cd a | git status"));
        // Setup alone is not a git command.
        assert!(!allowed("cd a && cd b"));
        assert!(!allowed("export X=1"));
        assert!(!allowed("ls && git status"));
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
        assert!(!allowed("git status\ncargo build"));
        assert!(!allowed("git status && /usr/bin/cargo build"));
        assert!(!allowed("git status && CARGO_TERM_COLOR=never cargo build"));
        assert!(!allowed("git status && cargo.exe build"));
        // Shells, interpreters and wrappers can hide a build in quoted text.
        assert!(!allowed("git status && bash -c 'cargo test'"));
        assert!(!allowed("git status && env cargo build"));
        assert!(!allowed("git status && python run_tests.py"));
        assert!(!allowed("git ls-files | xargs cargo fmt"));
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
}
