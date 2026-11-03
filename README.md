# peak-blocker

A Claude Code `PreToolUse` hook that pauses tool calls during Anthropic's weekday peak
window, when session limits drain faster.

**The window** is Monday to Friday, 05:00 to 11:00 US Pacific, which is Tuesday to Saturday
in New Zealand at hours that shift with both countries' daylight saving (the denial message
gives today's NZ hours). Outside it the hook does nothing. Inside it every tool call is
denied except `git` and `gh` commands (that run no build or test program such as `cargo` or
`pytest`) and a few tools that coordinate running agents, so agents commit, push and stop,
and nothing new starts.

It replaces a shell script that did the same thing with about ten subprocesses per tool
call. That script is kept in [`reference/`](reference/peak-hours-guard.sh) as the thing this
tool is checked against. A hook runs before every tool call, so its cost is paid hundreds of
times a session. On Windows, a call took about 410 ms with the script and about 35 ms with
this binary, which is the cost of starting any process there.

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

- `touch ~/.claude/allow-peak-hours` lets calls through for the peak window it was touched
  in, or for the next one if it was touched outside a window, and the guard re-arms at the
  window after that. Touch it during a window and it lasts until 11:00 Pacific; touch it the
  evening before and it covers the next morning. The file does not need removing, and a
  forgotten one from last week is ignored.
- `PEAK_HOURS_NOW_EPOCH=<unix seconds>` makes it behave as if it were that moment, which is
  how the tests and the comparison script drive it.

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

### What changed from the shell script

The script read the command with `sed`, took any build program's name anywhere in the text
as a build, and handled only one leading `cd`. Compared with it, this tool:

- parses the JSON, so quoted commit messages, heredocs and leading spaces are allowed;
- looks at the program a command runs, so `.claude/` and `.cargo/` paths, branch names and
  `--author=claude` are allowed;
- skips `NAME=value` and `export NAME=value;` before git or gh and any number of `cd`
  steps, and accepts a tab after `git`;
- denies a shell, interpreter or wrapper after the first git command (the script allowed
  `git status && python x.py`), because quoted text no longer counts as a build;
- gives the New Zealand hours for the time of year in the denial message (the script always
  said 01:00 to 07:00 NZDT);
- applies an override only to one window instead of until the file is removed.

`scripts/compare_with_script.py` asserts these as the only differences.

## Checking it

```sh
python scripts/gen_window_intervals.py   # regenerate the zoneinfo answers (needs tzdata on Windows)
cargo test                               # the window against Python's zoneinfo, hour by hour, 2024-2030
cargo build --release
python scripts/compare_with_script.py    # same decisions as the shell script, apart from the intended ones
```

`cargo test` checks the first and last second of every hour from 2024 to 2030 against
answers produced by Python's `zoneinfo`, an independent implementation, covering seven
spring and seven autumn clock changes. `compare_with_script.py` runs the binary and the
script on the edges of the window and a spread of tool calls and compares whether each call
is denied.
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
