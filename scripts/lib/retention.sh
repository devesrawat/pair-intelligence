#!/usr/bin/env bash
# Source me. Backup retention: keep the newest backup of each of the 7 most recent
# days, plus the newest backup of each of the 4 most recent ISO weeks that contain
# no daily keep (i.e. weeks older than the daily window). Everything else matching the pattern is deleted.
# Decisions depend only on file names (pair-YYYYMMDDTHHMMSSZ.dump[.age]), not on "now".

RETAIN_DAILY="${RETAIN_DAILY:-7}"
RETAIN_WEEKLY="${RETAIN_WEEKLY:-4}"

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
  while IFS= read -r f; do files+=("$f"); done < <(
    find "$dir" -maxdepth 1 -type f -name 'pair-????????T??????Z.dump*' ! -name '*.partial' | sort -r)
  local -a keep=()
  for f in "${files[@]+"${files[@]}"}"; do
    base="$(basename "$f")"; day="${base:5:8}"
    if [[ " ${keep_days[*]-} " != *" $day "* ]] && [ "${#keep_days[@]}" -lt "$RETAIN_DAILY" ]; then
      keep_days+=("$day"); keep+=("$f"); oldest_daily="$day"
      daily_weeks+=("$(iso_week "$day")")
    fi
  done
  for f in "${files[@]+"${files[@]}"}"; do
    base="$(basename "$f")"; day="${base:5:8}"
    [[ " ${keep[*]-} " == *" $f "* ]] && continue
    [ -n "$oldest_daily" ] && [ "$day" -ge "$oldest_daily" ] && continue
    week="$(iso_week "$day")"
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
  local dir="$1" f n=0
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    rm -f -- "$f"; n=$((n + 1))
  done < <(retention_victims "$dir")
  echo "retention: pruned $n file(s) in $dir"
}
