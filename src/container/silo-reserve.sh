#!/bin/sh
set -eu

runtime=${SILO_RUNTIME_DIR:-/run/silo}
lock="$runtime/session.lock"
reservations="$runtime/reservations"

if [ "$#" -ne 1 ]; then
    echo "usage: silo-reserve RESERVATION" >&2
    exit 64
fi

reservation=$1
case "$reservation" in
    *[!0-9a-f]*|'')
        echo "invalid silo session reservation" >&2
        exit 64
        ;;
esac

# Publish before taking the shared lock. If PID 1 already holds the exclusive
# lock it will either observe this reservation and yield, or exit; in the
# latter case the host safely restarts and retries this helper only.
temporary="$reservations/.$reservation.$$"
reservation_file="$reservations/$reservation"
expiry=$(($(date +%s) + 30))
printf '%s\n' "$expiry" > "$temporary"
mv -f "$temporary" "$reservation_file"

exec 9>"$lock"
flock --shared 9
