"""Check that the release binary decides as reference/peak-hours-guard.sh does.

The script is slow (it starts about ten subprocesses per call), so this runs about forty
cases, not a grid:

* the window edges: one tool call that is denied inside the window and allowed outside,
  at the instants either side of each edge, in both daylight-saving regimes, plus a
  Saturday and a Friday evening;
* the decision logic: many tool calls (git, gh, builds, escapes, `cd` prefixes, other
  tools) at one instant inside the window.

The clock arithmetic itself is covered, hour by hour, by `cargo test` against Python's
zoneinfo. This script compares the exit code and the output, then times a few calls.

    cargo build --release
    python scripts/compare_with_script.py

On Windows the script needs Git Bash, not the WSL `bash` that is first on the PATH. Set
GIT_BASH to its path if it is not under C:/Program Files/Git.
"""

import calendar
import datetime
import json
import os
import subprocess
import sys
import tempfile
import time

root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
script = os.path.join(root, "reference", "peak-hours-guard.sh")
binary = os.path.join(root, "target", "release", "peak-blocker" + (".exe" if os.name == "nt" else ""))
bash = os.environ.get("GIT_BASH") or (
    "C:/Program Files/Git/usr/bin/bash.exe" if os.name == "nt" else "bash"
)


def utc(y, mo, d, h, mi=0, s=0):
    return calendar.timegm(datetime.datetime(y, mo, d, h, mi, s).timetuple())


def payload(tool, command=None, spaced=False):
    body = {"session_id": "x", "tool_name": tool, "tool_input": ({"command": command} if command is not None else {})}
    return json.dumps(body, separators=(", ", ": ") if spaced else (",", ":"))


# Window edges. 13:00 UTC is 05:00 Pacific standard time and 12:00 UTC is 05:00 daylight time.
edge_instants = [
    utc(2026, 11, 4, 12, 59, 59), utc(2026, 11, 4, 13), utc(2026, 11, 4, 18, 59, 59), utc(2026, 11, 4, 19),
    utc(2026, 3, 9, 11, 59, 59), utc(2026, 3, 9, 12), utc(2026, 3, 9, 17, 59, 59), utc(2026, 3, 9, 18),
    utc(2026, 11, 7, 14),  # a Saturday in Pacific time
    utc(2026, 11, 6, 19),  # the Friday-evening edge
    utc(2028, 3, 12, 10),  # the spring change of a leap year
]
edge_call = payload("Bash", "ls")

# The decision logic, at one instant inside the window (Wednesday 08:00 Pacific).
inside = utc(2026, 11, 4, 16)
commands = [
    "git status", "cd /tmp && git log", "cd /tmp ; git log", "cd /tmp & git log", "cd /tmp | git log",
    "git log && cargo build", "cargo test", "gh pr view 5", 'git commit -m "tidy cargo notes"',
    "git", "gh", "  git status", "  cd /tmp && git log", "git log --grep=cargos", "git log my_cargo",
    "git status && echo claude", "cd  && git status", "cd /a/b && cd /c && git log",
    "git commit -F - <<EOF\nmessage\nEOF",
]
logic_calls = [payload("Bash", c) for c in commands]
logic_calls += [payload("Bash", "git status", spaced=True), payload("PowerShell", "git status"), payload("PowerShell", "dir")]
logic_calls += [payload(tool) for tool in ("Edit", "ToolSearch", "SendMessage", "ScheduleWakeup", "Agent")]
logic_calls += ['{"nothing":"useful"}', ""]

cases = [(epoch, edge_call) for epoch in edge_instants] + [(inside, call) for call in logic_calls]

# The script reads the command with sed, so it denies any git command whose JSON holds an
# escape (a quoted commit message, a heredoc) or that starts with spaces. The binary parses
# the JSON and allows them. These are the only intended differences.
git_commands_the_script_wrongly_denies = {
    payload("Bash", c)
    for c in (
        'git commit -m "tidy cargo notes"',
        "  git status",
        "git commit -F - <<EOF\nmessage\nEOF",
        "git commit -F - <<'EOF'\nCo-Authored-By: Claude\nEOF",
    )
}
logic_calls += list(git_commands_the_script_wrongly_denies - set(logic_calls))
cases = [(epoch, edge_call) for epoch in edge_instants] + [(inside, call) for call in logic_calls]

# A home directory with no override file, so the guard is not switched off.
home = tempfile.mkdtemp()
base_env = dict(os.environ, HOME=home, USERPROFILE=home)


def run(command, epoch, data):
    env = dict(base_env, PEAK_HOURS_NOW_EPOCH=str(epoch))
    done = subprocess.run(command, input=data.encode(), capture_output=True, env=env, timeout=60)
    return done.returncode, done.stdout.decode(errors="replace")


mismatches = 0
for epoch, data in cases:
    expected = run([bash, script], epoch, data)
    actual = run([binary], epoch, data)
    if data in git_commands_the_script_wrongly_denies:
        # Intended difference: the script denies, the binary allows with no output.
        if not (expected[1] and actual == (0, "")):
            mismatches += 1
            print("UNEXPECTED", data[:100].replace("\n", "\\n"), "script:", expected, "binary:", actual)
    elif expected != actual:
        mismatches += 1
        when = datetime.datetime.fromtimestamp(epoch, datetime.timezone.utc)
        print("MISMATCH", epoch, when, "input", data[:100].replace("\n", "\\n"))
        print("   script:", expected[0], expected[1][:70].replace("\n", " "))
        print("   binary:", actual[0], actual[1][:70].replace("\n", " "))
print("cases:", len(cases), "mismatches:", mismatches)

# Timing, outside the window, where neither reads its input: the common case.
outside = utc(2026, 11, 7, 14)
runs = 10
started = time.perf_counter()
for _ in range(runs):
    run([bash, script], outside, edge_call)
script_ms = 1000 * (time.perf_counter() - started) / runs
started = time.perf_counter()
for _ in range(runs):
    run([binary], outside, edge_call)
binary_ms = 1000 * (time.perf_counter() - started) / runs
print(
    "outside the window, mean of %d calls including Python's process start: script %.0f ms, binary %.0f ms (%.0fx)"
    % (runs, script_ms, binary_ms, script_ms / binary_ms)
)
sys.exit(1 if mismatches else 0)
