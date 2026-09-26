#!/bin/sh
set -eu

runtime=${SILO_RUNTIME_DIR:-/run/silo}
lock="$runtime/session.lock"
reservations="$runtime/reservations"
persistent="$runtime/persistent"

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
        exec 9>"$lock"
        attempt=0
        while [ ! -e "$runtime/armed" ]; do
            # Count under the lock so a renew cannot retire the only lease
            # this snapshot can see.
            if flock --exclusive --nonblock 9; then
                live=$(count_live_reservations)
                if [ "$live" -ne 0 ]; then
                    flock --unlock 9
                    sleep 0.1
                    continue
                fi
                attempt=$((attempt + 1))
                if [ "$attempt" -ge 100 ]; then
                    exit 0
                fi
                flock --unlock 9
            fi
            sleep 0.1
        done
        while :; do
            if [ -e "$persistent" ]; then
                sleep 0.1
                continue
            fi
            if flock --exclusive --nonblock 9; then
                if [ "$(count_live_reservations)" -eq 0 ] && [ ! -e "$persistent" ]; then
                    exit 0
                fi
                flock --unlock 9
            fi
            sleep 0.1
        done
        ;;
    reserve)
        [ "$#" -eq 1 ] || [ "$#" -eq 2 ] || exit 64
        previous=${2:-}
        case "$previous" in
            *[!0-9A-Za-z]*) exit 64 ;;
        esac
        temporary=$(mktemp "$reservations/.pending.XXXXXX")
        token=${temporary##*.pending.}
        # 30s lease. The host renews after 20s while an address is still missing.
        printf '%s\n' "$(($(date +%s) + 30))" > "$temporary"
        # Publish the new lease first, then drop the previous one while init
        # cannot be counting.
        mv -f "$temporary" "$reservations/$token"
        exec 9>"$lock"
        flock --shared 9
        if [ -n "$previous" ]; then
            rm -f "$reservations/$previous"
        fi
        printf '%s\n' "$token"
        ;;
    release)
        [ "$#" -eq 2 ] || exit 64
        token=$2
        case "$token" in
            *[!0-9A-Za-z]*|'') exit 64 ;;
        esac
        # A joined session holds the lock until its command exits. Deleting
        # this token must not wait for that.
        rm -f "$reservations/$token"
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
    persist)
        [ "$#" -eq 2 ] || exit 64
        token=$2
        case "$token" in
            *[!0-9A-Za-z]*|'') exit 64 ;;
        esac
        reservation_file="$reservations/$token"
        exec 9>"$lock"
        flock --shared 9
        [ -f "$reservation_file" ] || exit 75
        rm -f "$reservation_file"
        touch "$persistent" "$runtime/armed"
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
        echo 'usage: silo-lifecycle {init|reserve|session|persist|stop-guard|release}' >&2
        exit 64
        ;;
esac
