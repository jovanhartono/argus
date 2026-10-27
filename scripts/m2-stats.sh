#!/bin/zsh
# Each day's keystroke-to-results times from the app's own log: how many, typical (p50), 95%
# (p95) and slowest, then all the days together, which is what the M2 target (95% under 30 ms)
# is judged on.
# usage: m2-stats.sh [--since "YYYY-MM-DD[ HH:MM:SS]"]    default: the last 24 hours
set -euo pipefail

usage="usage: $0 [--since \"YYYY-MM-DD[ HH:MM:SS]\"]"
window=(--last 1d)
case "${1:-}" in
  --since)
    [[ $# -eq 2 && -n $2 ]] || { echo $usage >&2; exit 1 }
    window=(--start "$2")
    ;;
  "") ;;
  *) echo $usage >&2; exit 1 ;;
esac

/usr/bin/log show $window --style compact --predicate \
  'process == "Telescope" AND category == "timing" AND eventMessage BEGINSWITH "results drawn"' |
  awk '/results drawn/ { for (i = 1; i < NF; i++) if ($i == "drawn") { print $1, $(i + 1); print "~all", $(i + 1) } }' |
  LC_ALL=C sort -k1,1 -k2,2n |
  awk '
    # Nearest rank: the smallest time that q of the searches came in at or under.
    function at(q,   r) { r = int(q * n); if (r < q * n) r++; return times[r < 1 ? 1 : r] }
    function report(   label) {
      label = key
      if (key == "~all") { if (days < 2) return; label = "all days" }
      printf "%-10s %6d %8.1f %8.1f %8.1f\n", label, n, at(0.5), at(0.95), times[n]
    }
    BEGIN { printf "%-10s %6s %8s %8s %8s\n", "day", "count", "p50 ms", "p95 ms", "max ms" }
    $1 != key { if (n) report(); key = $1; n = 0; if (key != "~all") days++ }
    { times[++n] = $2 }
    END { if (n) report(); else print "no \"results drawn\" lines in this window" }
  '
