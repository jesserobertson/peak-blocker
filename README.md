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

Once a version is published to crates.io, `cargo install peak-blocker --locked --root ~/.claude/hooks`
does the same.

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
- The tool call is parsed as JSON from standard input. Outside the window the input is never
  read. Input that is not valid JSON names no tool and is denied inside the window.
- A shell command is allowed when it is a `git` or `gh` command (after an optional leading
  `cd <dir> &&`) that runs no build or test program: `cargo`, `rustc`, `pixi`, `mpirun`,
  `pytest`, `maturin`, `nextest` or `claude`.

### Commit messages and heredocs

The text of quoted strings and heredoc bodies is ignored when looking for those programs, so
`git commit -m "fix the cargo notes"` and a heredoc message with a `Co-Authored-By: Claude`
trailer are allowed. Anything that would run is still checked, including command
substitutions inside double quotes and unquoted heredocs:

```sh
git commit -m "fix the cargo notes"       # allowed
git commit -F - <<'EOF' ... Claude ... EOF # allowed
git log $(cargo build)                    # denied
git log "$(cargo build)"                  # denied
git commit -m x && cargo test             # denied
```

This is a cost guard for agents, not a sandbox: the command is read by a small scanner, not
a full shell parser.

### What changed from the shell script

The script read the command with `sed`, so it could not read a command whose JSON holds an
escape, and it denied every quoted commit message, every heredoc and every command with
leading spaces, even though they are git commands. This tool parses the JSON, so it allows
them. Those are the only intended differences, and `scripts/compare_with_script.py` asserts
exactly that.

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

## Releasing

CI runs the formatter, clippy and the tests on Linux and Windows. Publishing is a tag:

1. Add a crates.io API token with the publish scope as the repository secret
   `CARGO_REGISTRY_TOKEN`.
2. Set the version in `Cargo.toml`, commit, then `git tag v<version>` and `git push --tags`.

The publish workflow refuses a tag that does not match the version in `Cargo.toml`, runs the
tests, and runs `cargo publish --locked`.

## License

MIT. See [LICENSE](LICENSE).
