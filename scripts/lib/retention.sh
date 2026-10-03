#!/usr/bin/env bash
# Source me. Backup retention: keep the newest backup of each of the 7 most recent
# days, plus the newest backup of each of the 4 most recent ISO weeks that contain
# no daily keep (i.e. weeks older than the daily window). Everything else matching the pattern is deleted.
# Decisions depend only on file names (pair-YYYYMMDDTHHMMSSZ.dump[.age]), not on "now".

RETAIN_DAILY="${RETAIN_DAILY:-7}"
RETAIN_WEEKLY="${RETAIN_WEEKLY:-4}"
# Exact names produced by scripts/backup; nothing else in the directory is ever a prune candidate.
BACKUP_NAME_RE='^pair-[0-9]{8}T[0-9]{6}Z\.dump(\.age)?$'

# ISO year-week for a YYYYMMDD day, portable across BSD and GNU date.
iso_week() {
  if date --version >/dev/null 2>&1; then
    date -u -d "${1:0:4}-${1:4:2}-${1:6:2}" +%G-%V
  else
    date -u -j -f %Y%m%d "$1" +%G-%V
  fi
}

# Prints the files to DELETE (one per line) for the backup directory $1.
retention_victims() {
  local dir="$1" f base day week
  local -a files=() keep_days=() keep_weeks=()
  local oldest_daily="" kept
  local -a daily_weeks=()
  local listing
  # Callers run this inside `$(...)`, where `set -e` is inactive: every failure must `return 1`.
  listing="$(find "$dir" -maxdepth 1 -type f -name 'pair-*' | sort -r; exit "${PIPESTATUS[0]}")" || return 1
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    # Exact backup names only; anything else (.bak, .partial, look-alikes) is never a candidate.
    [[ "$(basename "$f")" =~ $BACKUP_NAME_RE ]] && files+=("$f")
  done <<< "$listing"
  local -a keep=()
  for f in "${files[@]+"${files[@]}"}"; do
    base="$(basename "$f")"; day="${base:5:8}"
    if [[ " ${keep_days[*]-} " != *" $day "* ]] && [ "${#keep_days[@]}" -lt "$RETAIN_DAILY" ]; then
      week="$(iso_week "$day")" || return 1
      keep_days+=("$day"); keep+=("$f"); oldest_daily="$day"
      daily_weeks+=("$week")
    fi
  done
  for f in "${files[@]+"${files[@]}"}"; do
    base="$(basename "$f")"; day="${base:5:8}"
    [[ " ${keep[*]-} " == *" $f "* ]] && continue
    [ -n "$oldest_daily" ] && [ "$day" -ge "$oldest_daily" ] && continue
    week="$(iso_week "$day")" || return 1
    [[ " ${daily_weeks[*]-} " == *" $week "* ]] && continue
    if [[ " ${keep_weeks[*]-} " != *" $week "* ]] && [ "${#keep_weeks[@]}" -lt "$RETAIN_WEEKLY" ]; then
      keep_weeks+=("$week"); keep+=("$f")
    fi
  done
  for f in "${files[@]+"${files[@]}"}"; do
    kept=0
    [[ " ${keep[*]-} " == *" $f "* ]] && kept=1
    [ "$kept" -eq 0 ] && echo "$f"
  done
  return 0
}

retention_prune() {
  local dir="$1" f n=0 victims
  # Compute the full victim list first and check its status: a failure here must stop the prune
  # loudly rather than silently deleting nothing (or something partial).
  if ! victims="$(retention_victims "$dir")"; then
    echo "retention: FATAL could not compute prune list for $dir; nothing deleted" >&2
    return 1
  fi
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    rm -f -- "$f" || { echo "retention: FATAL could not delete $f" >&2; return 1; }
    n=$((n + 1))
  done <<< "$victims"
  echo "retention: pruned $n file(s) in $dir"
}
