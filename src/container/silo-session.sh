#!/bin/sh
set -eu

runtime=${SILO_RUNTIME_DIR:-/run/silo}
lock="$runtime/session.lock"

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
