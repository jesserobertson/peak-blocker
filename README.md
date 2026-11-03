# peak-blocker

A Claude Code `PreToolUse` hook that pauses tool calls during Anthropic's weekday peak
window, when session limits drain faster.

**The window** is Monday to Friday, 05:00 to 11:00 US Pacific, which is Tuesday to Saturday
in New Zealand at hours that shift with both countries' daylight saving (the denial message
gives today's NZ hours). Outside it the hook does nothing. Inside it every tool call is
denied except:

- `git` and `gh` commands that run no build or test program such as `cargo` or `pytest`;
- the `Write` and `Edit` tools, for files inside one report directory;
- a few tools that coordinate running agents.

So agents commit, push, record their state and stop, and nothing new starts.

A hook runs before every tool call, so it has to be cheap. This is one small binary that
starts no subprocess. On Windows a call takes about 35 ms, which is the cost of starting any
process there.

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

## Report files

During the window the file-writing tools (`Write`, `Edit`, `MultiEdit`) may write inside one
directory and nowhere else: `~/.claude/peak-reports/`, or the directory named by
`PEAK_BLOCKER_REPORT_DIR`. The denial message tells the agent where it is. Bash is still
limited to git and gh, so `echo >> file` is not a way around it.

The check is on whole path parts, after resolving `.` and `..`, and the path must be absolute.
`peak-reports-other/` is not inside `peak-reports/`, and `peak-reports/../x` is not either.
On Windows the comparison ignores case and accepts either slash.

## Override and testing

- `touch ~/.claude/allow-peak-hours` lets calls through for the peak window it was touched
  in, or for the next one if it was touched outside a window, and the guard re-arms at the
  window after that. Touch it during a window and it lasts until 11:00 Pacific; touch it the
  evening before and it covers the next morning. The file does not need removing, and a
  forgotten one from last week is ignored.
- `PEAK_HOURS_NOW_EPOCH=<unix seconds>` makes it behave as if it were that moment, for
  trying it out.

## How it decides

- The time comes from the IANA timezone database for `America/Los_Angeles`, bundled into the
  binary by [`jiff`](https://crates.io/crates/jiff). Daylight saving follows the database,
  not rules written in this repository.
- The tool call is parsed as JSON from standard input. Outside the window the input is never
  read. Input that is not valid JSON names no tool and is denied inside the window.
- A shell command is split into its simple commands (at `;`, `&&`, `||`, `|`, newlines,
  parentheses and backticks). Leading `cd`, `export` and `NAME=value` setup is skipped. The
  first command after that must be `git` or `gh`. No command may run a build or test program
  (`cargo`, `rustc`, `pixi`, `mpirun`, `pytest`, `maturin`, `nextest`, `claude`) or a shell,
  interpreter or wrapper that could hide one (`bash`, `python`, `node`, `env`, `xargs`, ...).
  Only the program a command runs counts. A build program's name in a path, a branch or an
  option does not.

### Commit messages and heredocs

The text of quoted strings and heredoc bodies is not code, so
`git commit -m "fix the cargo notes"` and a heredoc message with a `Co-Authored-By: Claude`
trailer are allowed. Anything that would run is still checked, including command
substitutions inside double quotes and unquoted heredocs:

```sh
git add .claude/settings.json             # allowed: a path, not a program
git commit -m "fix the cargo notes"       # allowed
git commit -F - <<'EOF' ... Claude ... EOF # allowed
GH_TOKEN=$(gh auth token) gh issue edit 5 # allowed
git log $(cargo build)                    # denied
git log "$(cargo build)"                  # denied
git commit -m x && cargo test             # denied
git status && bash -c 'cargo test'        # denied
```

This is a cost guard for agents, not a sandbox: the command is read by a small scanner, not
a full shell parser.

## Tests

```sh
cargo test
python scripts/gen_window_intervals.py   # regenerate the zoneinfo answers (needs tzdata on Windows)
```

`cargo test` checks the first and last second of every hour from 2024 to 2030 against
answers produced by Python's `zoneinfo`, an independent implementation of the timezone
rules, covering seven spring and seven autumn clock changes. It also covers the command
rules, the report directory, the override, the New Zealand hours in the message, and that the
command scanner cannot panic on any short run of quotes, heredocs and substitutions.

## Releasing

CI runs the formatter, clippy and the tests on Linux and Windows. Publishing is a tag:

1. Add a crates.io API token with the publish scope as the repository secret
   `CARGO_REGISTRY_TOKEN`.
2. Set the version in `Cargo.toml`, commit, then `git tag v<version>` and `git push --tags`.

The publish workflow refuses a tag that does not match the version in `Cargo.toml`, runs the
tests, and runs `cargo publish --locked`.

## License

MIT. See [LICENSE](LICENSE).
