#!/bin/sh
set -eu

runtime=${SILO_RUNTIME_DIR:-/run/silo}
lock="$runtime/session.lock"
reservations="$runtime/reservations"

count_live_reservations() {
    now=$(date +%s)
    count=0
    for reservation in "$reservations"/*; do
        [ -e "$reservation" ] || continue
        expiry=
        IFS= read -r expiry < "$reservation" || true
        case "$expiry" in
            *[!0-9]*|'')
                rm -f "$reservation"
                continue
                ;;
        esac
        if [ "$expiry" -ge "$now" ]; then
            count=$((count + 1))
        else
            rm -f "$reservation"
        fi
    done
    printf '%s\n' "$count"
}

# The host keeps stdin open while stopping the container. Holding the
# exclusive lifecycle lock closes the gap between the session-count probe and
# `container stop`. Reservations bridge the handoff between the reserver and
# session wrapper, so check them while the lock is held before allowing stop.
exec 9>"$lock"
if ! flock --exclusive --nonblock 9; then
    exit 75
fi
reservation_count=$(count_live_reservations)
if [ "$reservation_count" -ne 0 ]; then
    printf '%s\n' "$reservation_count" >&2
    exit 76
fi
printf 'ready\n'
cat >/dev/null
