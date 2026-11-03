//! A Claude Code `PreToolUse` hook that pauses tool calls during Anthropic's weekday
//! peak window (Mon-Fri 05:00-11:00 US Pacific, about 01:00-07:00 NZDT Tue-Sat),
//! when session limits drain faster.
//!
//! Inside the window every tool call is denied except git and gh commands and a
//! few coordination tools, so running agents commit, push and stop, and nothing
//! new starts. The Pacific time comes from the IANA timezone database bundled by
//! `jiff`, so daylight saving follows the database, not rules written here. The
//! decisions match `reference/peak-hours-guard.sh`, the shell script this
//! replaces, without starting a subprocess.
//!
//! Override: create `~/.claude/allow-peak-hours` to disable the guard until the
//! file is removed. Testing: `PEAK_HOURS_NOW_EPOCH=<unix seconds>` pretends it
//! is that moment.

use std::io::Read;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use jiff::Timestamp;

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

/// Programs that mean a build or a test run, which git and gh commands must not mention.
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

/// The value of the last `"key": "value"` pair on `line` whose value reaches its
/// closing quote without meeting a byte in `forbidden`. This reads the hook input
/// the way the shell script's `sed` does: no JSON parsing, so a value that holds
/// an escape (a backslash, when `forbidden` has one) is not found at all, and an
/// earlier pair with the same key is tried instead.
fn last_string_value(line: &str, key: &str, forbidden: &[u8]) -> Option<String> {
    let needle = format!("\"{key}\"");
    let bytes = line.as_bytes();
    let mut search_end = line.len();
    while let Some(at) = line[..search_end].rfind(&needle) {
        let mut i = at + needle.len();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b':' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'"' {
                let start = i + 1;
                for (offset, byte) in bytes[start..].iter().enumerate() {
                    if *byte == b'"' {
                        return Some(line[start..start + offset].to_string());
                    }
                    if forbidden.contains(byte) {
                        break;
                    }
                }
            }
        }
        search_end = at;
    }
    None
}

/// The first line of `input` that carries `key`, as `last_string_value` reads it.
fn field(input: &str, key: &str, forbidden: &[u8]) -> Option<String> {
    input
        .lines()
        .find_map(|line| last_string_value(line, key, forbidden))
}

/// `command` without a leading `cd <dir> &&` or `cd <dir> ;`. A command without
/// that prefix is returned untouched, leading spaces included.
fn without_leading_cd(command: &str) -> &str {
    let Some(rest) = command.trim_start().strip_prefix("cd ") else {
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

/// Whether the tool call is allowed during the peak window.
fn allowed_in_window(input: &str) -> bool {
    let tool = field(input, "tool_name", b"").unwrap_or_default();
    if ALWAYS_ALLOWED.contains(&tool.as_str()) {
        return true;
    }
    if tool == "Bash" || tool == "PowerShell" {
        let command = field(input, "command", b"\\").unwrap_or_default();
        let stripped = without_leading_cd(&command);
        let is_git = stripped == "git"
            || stripped == "gh"
            || stripped.starts_with("git ")
            || stripped.starts_with("gh ");
        return is_git && !mentions_build_program(&command);
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

    /// "<start> <end>" lines, in Unix seconds with the end excluded, for each stretch of
    /// hours inside the window from 2024 to 2030, written by Python's `zoneinfo` from
    /// the IANA database (`scripts/gen_window_intervals.py`).
    const ZONEINFO_WINDOWS: &str = include_str!("../tests/pacific_window_intervals.txt");

    /// 2024-01-01T00:00:00Z and 2031-01-01T00:00:00Z.
    const FIRST_SECOND: i64 = 1_704_067_200;
    const END_SECOND: i64 = 1_924_992_000;

    fn call(tool: &str, command: &str) -> String {
        format!("{{\"tool_name\":\"{tool}\",\"tool_input\":{{\"command\":\"{command}\"}}}}")
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
        assert!(allowed_in_window(&call("Bash", "git status")));
        assert!(allowed_in_window(&call("Bash", "cd /tmp && git log")));
        assert!(allowed_in_window(&call("Bash", "gh pr view 5")));
        assert!(!allowed_in_window(&call("Bash", "git log && cargo build")));
        assert!(!allowed_in_window(&call("Bash", "cargo test")));
        assert!(!allowed_in_window(&call("Bash", "ls")));
        assert!(allowed_in_window(&call("ToolSearch", "")));
        assert!(!allowed_in_window(&call("Edit", "")));
    }

    #[test]
    fn a_command_the_script_cannot_read_is_denied_as_the_script_denies_it() {
        // The script reads the command with sed and no JSON parser, so a command
        // holding an escape (an escaped quote, or a \n in a heredoc) is not found,
        // and the call is denied. The binary keeps that behaviour.
        assert!(!allowed_in_window(&call("Bash", r#"git commit -m \"x\""#)));
        assert!(!allowed_in_window(&call(
            "Bash",
            r"git commit -F - <<EOF\nmsg\nEOF"
        )));
        // Leading spaces are not trimmed unless a `cd` prefix is stripped.
        assert!(!allowed_in_window(&call("Bash", "  git status")));
        assert!(allowed_in_window(&call("Bash", "  cd /tmp && git status")));
    }

    #[test]
    fn a_program_name_inside_a_longer_word_is_not_a_build() {
        assert!(allowed_in_window(&call("Bash", "git log --grep=cargos")));
        assert!(allowed_in_window(&call("Bash", "git log my_cargo")));
    }
}
