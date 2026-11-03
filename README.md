# peak-blocker

A Claude Code `PreToolUse` hook that pauses tool calls during Anthropic's weekday peak
window, when session limits drain faster.

**The window** is Monday to Friday, 05:00 to 11:00 US Pacific (about 01:00 to 07:00 NZDT,
Tuesday to Saturday). Outside it the hook does nothing. Inside it every tool call is denied
except `git` and `gh` commands (never ones that mention a build or test program such as
`cargo` or `pytest`) and a few tools that coordinate running agents, so agents commit, push
and stop, and nothing new starts.

It replaces a shell script that did the same thing with about ten subprocesses per tool
call. That script is kept in [`reference/`](reference/peak-hours-guard.sh) as the thing this
tool is checked against. A hook runs before every tool call, so its cost is paid
hundreds of times a session.

## Install

```sh
cargo install --git https://github.com/jesserobertson/peak-blocker --locked --root ~/.claude/hooks
```

This puts the binary at `~/.claude/hooks/bin/peak-blocker` (`peak-blocker.exe` on Windows).
Then point the hook at it in `~/.claude/settings.json`:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "hooks": [
          { "type": "command", "command": "~/.claude/hooks/bin/peak-blocker", "timeout": 10 }
        ]
      }
    ]
  }
}
```

To manage it with [chezmoi](https://www.chezmoi.io/), run the `cargo install` line from a
`run_onchange_after_` script in your dotfiles, and keep the settings entry above in your
templated `settings.json`.

## Override and testing

- `touch ~/.claude/allow-peak-hours` switches the guard off until the file is removed.
- `PEAK_HOURS_NOW_EPOCH=<unix seconds>` makes it behave as if it were that moment, which is
  how the tests and the comparison script drive it.

## How it decides

- The time comes from the IANA timezone database for `America/Los_Angeles`, bundled into the
  binary by [`jiff`](https://crates.io/crates/jiff). Daylight saving follows the database,
  not rules written in this repository.
- The tool call is read from the JSON on standard input without a JSON parser, the way the
  shell script's `sed` reads it. Outside the window the input is never read.

### Known behaviour inherited from the script

A command that contains an escape in the JSON, such as a quoted commit message
(`git commit -m "..."`, which arrives as `\"`) or a heredoc (`\n`), cannot be read that way,
so during the window it is **denied** even though it is a git command. A command with
leading spaces is also denied. This matches the script on purpose, so that the binary is a
drop-in replacement. Both are probably bugs, and parsing the JSON properly is the fix.

## Checking it

```sh
python scripts/gen_window_intervals.py   # regenerate the zoneinfo answers (needs tzdata on Windows)
cargo test                               # the window against Python's zoneinfo, hour by hour, 2024-2030
cargo build --release
python scripts/compare_with_script.py    # same decisions as the shell script, and the timing
```

`cargo test` checks the first and last second of every hour from 2024 to 2030 against
answers produced by Python's `zoneinfo`, an independent implementation, covering seven
spring and seven autumn clock changes. `compare_with_script.py` runs the binary and the
script on the edges of the window and a spread of tool calls and compares their output.
On Windows the script needs Git Bash, not the WSL `bash`; set `GIT_BASH` if it is not under
`C:/Program Files/Git`.

## License

MIT. See [LICENSE](LICENSE).
