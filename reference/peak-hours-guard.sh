#!/usr/bin/env bash
# PreToolUse guard for Anthropic's weekday peak window (Mon-Fri 05:00-11:00 US Pacific,
# about 01:00-07:00 NZDT Tue-Sat), when session limits drain faster.
#
# Inside the window every tool call is denied except git and gh commands and a few
# coordination tools, so running agents commit, push and stop, and nothing new starts.
# The window is computed from UTC with the US daylight-saving rules, so it moves with them.
#
# Override: `touch ~/.claude/allow-peak-hours` disables the guard until the file is removed.
# Testing: PEAK_HOURS_NOW_EPOCH=<unix seconds> pretends it is that moment.

input=$(cat)

[ -e "$HOME/.claude/allow-peak-hours" ] && exit 0

now=${PEAK_HOURS_NOW_EPOCH:-$(date -u +%s)}
year=$(date -u -d "@$now" +%Y)

# US daylight saving: second Sunday of March 02:00 PST (10:00 UTC) to first Sunday of
# November 02:00 PDT (09:00 UTC).
mar_dow=$(date -u -d "$year-03-01" +%u)
nov_dow=$(date -u -d "$year-11-01" +%u)
mar_day=$(( 1 + (7 - mar_dow) % 7 + 7 ))
nov_day=$(( 1 + (7 - nov_dow) % 7 ))
dst_start=$(date -u -d "$year-03-$(printf %02d "$mar_day") 10:00:00" +%s)
dst_end=$(date -u -d "$year-11-$(printf %02d "$nov_day") 09:00:00" +%s)
if [ "$now" -ge "$dst_start" ] && [ "$now" -lt "$dst_end" ]; then offset=-7; else offset=-8; fi

pacific=$(( now + offset * 3600 ))
weekday=$(date -u -d "@$pacific" +%u)
hour=$(( 10#$(date -u -d "@$pacific" +%H) ))

# Outside Mon-Fri 05:00-11:00 Pacific: allow everything.
if [ "$weekday" -gt 5 ] || [ "$hour" -lt 5 ] || [ "$hour" -ge 11 ]; then exit 0; fi

tool=$(printf '%s' "$input" | sed -n 's/.*"tool_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1)

case "$tool" in
  SendMessage|ListAgents|TaskStop|ReadNotifications|CronList|CronCreate|CronDelete|ScheduleWakeup|ToolSearch|TaskOutput)
    exit 0 ;;
  Bash|PowerShell)
    command=$(printf '%s' "$input" | sed -n 's/.*"command"[[:space:]]*:[[:space:]]*"\(\([^"\]\|\.\)*\)".*/\1/p' | head -n1)
    # Allow git and gh (optionally after a leading cd), but never builds or test runs.
    stripped=$(printf '%s' "$command" | sed -E 's/^[[:space:]]*cd [^&;|]+(&&|;)[[:space:]]*//')
    case "$stripped" in
      git\ *|gh\ *|git|gh)
        if ! printf '%s' "$command" | grep -Eq '(^|[^a-z_])(cargo|rustc|pixi|mpirun|pytest|maturin|nextest|claude)([^a-z_]|$)'; then exit 0; fi ;;
    esac ;;
esac

reason="Peak-hour pause (Mon-Fri 05:00-11:00 US Pacific, about 01:00-07:00 NZDT): Claude limits drain faster now. Commit and push your work with git, append your state to your report file, then stop and wait; do not retry other tools. Override: touch ~/.claude/allow-peak-hours"
printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"%s"}}\n' "$reason"
exit 0
