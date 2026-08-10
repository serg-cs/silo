#!/bin/sh
set -eu

runtime=${SILO_RUNTIME_DIR:-/run/silo}
armed="$runtime/armed"
lock="$runtime/session.lock"
reservations="$runtime/reservations"

trap 'exit 0' HUP INT QUIT TERM

has_live_reservation() {
    now=$(date +%s)
    live=false
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
            live=true
        else
            rm -f "$reservation"
        fi
    done
    [ "$live" = true ]
}

# A newly created or restarted container gets a bounded window for the host
# to attach its first session. This avoids leaking a VM if host-side exec
# fails after creation/start but before the guest wrapper can arm lifecycle.
attempt=0
while [ ! -e "$armed" ]; do
    if has_live_reservation; then
        sleep 0.1
        continue
    fi
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 100 ]; then
        exit 0
    fi
    sleep 0.1
done

# The wrapper holds shared locks. Once every wrapper and inheriting child has
# closed its descriptor, the exclusive lock succeeds and PID 1 exits.
exec 9>"$lock"
while :; do
    if flock --exclusive --nonblock 9; then
        if ! has_live_reservation; then
            exit 0
        fi
        flock --unlock 9
    fi
    sleep 0.1
done
