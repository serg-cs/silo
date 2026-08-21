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
            *[!0-9]*|'') rm -f "$reservation"; continue ;;
        esac
        if [ "$expiry" -ge "$now" ]; then
            count=$((count + 1))
        else
            rm -f "$reservation"
        fi
    done
    printf '%s\n' "$count"
}

case "${1:-}" in
    init)
        trap 'exit 0' HUP INT QUIT TERM
        attempt=0
        while [ ! -e "$runtime/armed" ]; do
            if [ "$(count_live_reservations)" -ne 0 ]; then
                sleep 0.1
                continue
            fi
            attempt=$((attempt + 1))
            [ "$attempt" -lt 100 ] || exit 0
            sleep 0.1
        done
        exec 9>"$lock"
        while :; do
            if flock --exclusive --nonblock 9; then
                if [ "$(count_live_reservations)" -eq 0 ]; then
                    exit 0
                fi
                flock --unlock 9
            fi
            sleep 0.1
        done
        ;;
    reserve)
        [ "$#" -eq 1 ] || exit 64
        temporary=$(mktemp "$reservations/.pending.XXXXXX")
        token=${temporary##*.pending.}
        printf '%s\n' "$(($(date +%s) + 30))" > "$temporary"
        mv -f "$temporary" "$reservations/$token"
        printf '%s\n' "$token"
        exec 9>"$lock"
        flock --shared 9
        ;;
    session)
        [ "$#" -ge 3 ] || exit 64
        token=$2
        shift 2
        case "$token" in
            *[!0-9A-Za-z]*|'') exit 64 ;;
        esac
        reservation_file="$reservations/$token"
        exec 9>"$lock"
        flock --shared 9
        [ -f "$reservation_file" ] || exit 75
        rm -f "$reservation_file"
        touch "$runtime/armed"
        exec "$@"
        ;;
    stop-guard)
        [ "$#" -eq 1 ] || exit 64
        exec 9>"$lock"
        flock --exclusive --nonblock 9 || exit 75
        reservations_count=$(count_live_reservations)
        if [ "$reservations_count" -ne 0 ]; then
            printf '%s\n' "$reservations_count" >&2
            exit 76
        fi
        printf 'ready\n'
        cat >/dev/null
        ;;
    *)
        echo 'usage: silo-lifecycle {init|reserve|session|stop-guard}' >&2
        exit 64
        ;;
esac
