#!/bin/sh
set -eu

runtime=${SILO_RUNTIME_DIR:-/run/silo}
pids=
logs=

usage() {
    echo "usage: silo-forward HOST LOCAL_PORT:RELAY_PORT [...] -- COMMAND [ARG...]" >&2
    exit 64
}

cleanup() {
    for pid in $pids; do
        kill "$pid" 2>/dev/null || true
    done
    for pid in $pids; do
        wait "$pid" 2>/dev/null || true
    done
    for log in $logs; do
        rm -f "$log"
    done
}

start_relay() {
    family=$1
    local_port=$2
    host=$3
    relay_port=$4
    log="$runtime/forward-$local_port-$family.log"
    logs="$logs $log"
    if [ "$family" = 4 ]; then
        socat \
            "TCP4-LISTEN:$local_port,bind=127.0.0.1,reuseaddr,fork" \
            "TCP4:$host:$relay_port" 2>"$log" &
    else
        socat \
            "TCP6-LISTEN:$local_port,bind=[::1],ipv6only=1,reuseaddr,fork" \
            "TCP4:$host:$relay_port" 2>"$log" &
    fi
    pids="$pids $!"
}

[ "$#" -ge 4 ] || usage
host=$1
shift
case "$host" in
    *[!0-9.]*|'') usage ;;
esac

# Validate the host-provided map before starting any persistent listener.
seen=
mapping_count=0
mappings=
while [ "$#" -gt 0 ] && [ "$1" != -- ]; do
    mapping=$1
    shift
    case "$mapping" in
        *:*:*) usage ;;
        [0-9]*:[0-9]*) ;;
        *) usage ;;
    esac
    local_port=${mapping%%:*}
    relay_port=${mapping#*:}
    case "$local_port:$relay_port" in
        *[!0-9:]*) usage ;;
    esac
    [ "$local_port" -ge 1024 ] && [ "$local_port" -le 65535 ] || usage
    [ "$relay_port" -ge 1 ] && [ "$relay_port" -le 65535 ] || usage
    case " $seen " in
        *" $local_port "*) usage ;;
    esac
    seen="$seen $local_port"
    mapping_count=$((mapping_count + 1))
    mappings="$mappings $mapping"
done
[ "$mapping_count" -gt 0 ] || usage
[ "$#" -ge 2 ] && [ "$1" = -- ] || usage
shift

# Start the validated map, then fail readiness unless every listener survives.
trap cleanup EXIT HUP INT QUIT TERM
for mapping in $mappings; do
    local_port=${mapping%%:*}
    relay_port=${mapping#*:}
    start_relay 4 "$local_port" "$host" "$relay_port"
    start_relay 6 "$local_port" "$host" "$relay_port"
done
sleep 0.1
for pid in $pids; do
    if ! kill -0 "$pid" 2>/dev/null; then
        for log in $logs; do
            [ ! -s "$log" ] || cat "$log" >&2
        done
        exit 1
    fi
done
for log in $logs; do
    rm -f "$log"
done
touch "$runtime/ready"

# A successful exec clears the shell traps; the container runtime owns relay cleanup.
exec "$@"
