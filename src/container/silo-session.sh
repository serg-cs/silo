#!/bin/sh
set -eu

runtime=${SILO_RUNTIME_DIR:-/run/silo}
lock="$runtime/session.lock"
sessions="$runtime/sessions"

if [ "$#" -lt 2 ]; then
    echo "usage: silo-session RESERVATION COMMAND [ARG...]" >&2
    exit 64
fi

reservation=$1
shift
case "$reservation" in
    *[!0-9a-f]*|'')
        echo "invalid silo session reservation" >&2
        exit 64
        ;;
esac
reservation_file="$runtime/reservations/$reservation"

# Publish a per-session lease only after its shared lock is held. The command
# and all descendants inherit descriptor 8, so the marker stays observably
# active for exactly as long as the lifecycle lease on descriptor 9.
temporary_session="$sessions/.$reservation.$$"
session_file="$sessions/$reservation"
: > "$temporary_session"
exec 8<>"$temporary_session"
flock --shared 8
mv -f "$temporary_session" "$session_file"

# Keep this descriptor inheritable: background descendants intentionally keep
# the container alive until they also exit or explicitly close descriptor 9.
exec 9>"$lock"
flock --shared 9
if [ ! -f "$reservation_file" ]; then
    echo "silo session reservation expired" >&2
    exit 75
fi
rm -f "$reservation_file"
touch "$runtime/armed"

exec "$@"
