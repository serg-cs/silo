#!/bin/sh
set -eu

runtime=${SILO_RUNTIME_DIR:-/run/silo}
sessions="$runtime/sessions"
count=0

for session in "$sessions"/*; do
    [ -e "$session" ] || continue
    exec 8<>"$session"
    if flock --exclusive --nonblock 8; then
        rm -f "$session"
    else
        count=$((count + 1))
    fi
    exec 8>&-
done

printf '%s\n' "$count"
