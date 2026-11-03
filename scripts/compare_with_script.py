"""Check that the release binary decides as reference/peak-hours-guard.sh does, except where
it is meant to differ.

The script is slow (it starts about ten subprocesses per call), so this runs about fifty
cases, not a grid:

* the window edges: one tool call that is denied inside the window and allowed outside,
  at the instants either side of each edge, in both daylight-saving regimes, plus a
  Saturday and a Friday evening;
* the decision logic: many tool calls at one instant inside the window, each in one of
  three buckets: the two agree; the script wrongly denies a git command that the binary
  allows; the binary is deliberately stricter than the script.

The clock arithmetic is covered, hour by hour, by `cargo test` against Python's zoneinfo.
This script compares the exit code and whether the call was denied, not the message text,
which differs because the binary gives the New Zealand hours for the time of year.

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

# The two agree.
same = [
    "git status", "cd /tmp && git log", "cd /tmp ; git log", "gh pr view 5", "git", "gh",
    "git log && cargo build", "cargo test", "ls", "git log --grep=cargos", "git log my_cargo",
    "cd  && git status", "  cd /tmp && git log", "git status | head",
]
# The script wrongly denies a git command; the binary allows it. The script reads the command
# with sed, so it cannot read a JSON escape (a quoted commit message, a heredoc), it takes
# any build program name anywhere as a build (a path, a branch, an option), it does not skip
# variable assignments, `export` or more than one `cd`, and it needs a space after `git`.
script_wrongly_denies = [
    'git commit -m "tidy cargo notes"',
    "  git status",
    "git commit -F - <<EOF\nmessage\nEOF",
    "git commit -F - <<'EOF'\nCo-Authored-By: Claude\nEOF",
    "git add .claude/settings.json",
    "git checkout -b feature/pytest-fixes",
    "git log --author=claude",
    "git status && echo claude",
    "GH_TOKEN=x gh issue edit 5",
    "export GH_TOKEN=x; gh issue edit 5",
    "cd a && cd b && git status",
    "cd /tmp & git log",
    "cd /tmp | git log",
    "git\tstatus",
]
# The binary is deliberately stricter: a shell, interpreter or wrapper after git can hide a build.
binary_stricter = [
    "git status && python run_tests.py",
    "git status && bash -c 'ls'",
    "git ls-files | xargs ls",
]
others = [
    payload("Bash", "git status", spaced=True), payload("PowerShell", "git status"), payload("PowerShell", "dir"),
    payload("Edit"), payload("ToolSearch"), payload("SendMessage"), payload("ScheduleWakeup"), payload("Agent"),
    '{"nothing":"useful"}', "",
]

cases = [(epoch, edge_call, "same") for epoch in edge_instants]
cases += [(inside, payload("Bash", c), "same") for c in same]
cases += [(inside, payload("Bash", c), "script_wrongly_denies") for c in script_wrongly_denies]
cases += [(inside, payload("Bash", c), "binary_stricter") for c in binary_stricter]
cases += [(inside, data, "same") for data in others]

# A home directory with no override file, so the guard is not switched off.
home = tempfile.mkdtemp()
base_env = dict(os.environ, HOME=home, USERPROFILE=home)


def decide(command, epoch, data):
    """The exit code and whether the call was denied."""
    env = dict(base_env, PEAK_HOURS_NOW_EPOCH=str(epoch))
    done = subprocess.run(command, input=data.encode(), capture_output=True, env=env, timeout=60)
    return done.returncode, '"permissionDecision":"deny"' in done.stdout.decode(errors="replace")


# What each bucket expects of (script, binary), as (exit code, denied).
expected_pair = {
    "script_wrongly_denies": ((0, True), (0, False)),
    "binary_stricter": ((0, False), (0, True)),
}

failures = 0
for epoch, data, bucket in cases:
    from_script = decide([bash, script], epoch, data)
    from_binary = decide([binary], epoch, data)
    ok = from_script == from_binary if bucket == "same" else (from_script, from_binary) == expected_pair[bucket]
    if not ok:
        failures += 1
        when = datetime.datetime.fromtimestamp(epoch, datetime.timezone.utc)
        print("FAIL [%s]" % bucket, when, data[:100].replace("\n", "\\n"))
        print("   script (exit, denied):", from_script)
        print("   binary (exit, denied):", from_binary)
print("cases:", len(cases), "failures:", failures)
sys.exit(1 if failures else 0)
