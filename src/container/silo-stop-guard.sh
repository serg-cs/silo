#!/bin/sh
set -eu

runtime=${SILO_RUNTIME_DIR:-/run/silo}
lock="$runtime/session.lock"

# The host keeps stdin open while stopping the container. Holding the
# exclusive lifecycle lock closes the gap between the session-count probe and
# `container stop`; new reservations can continue if stopping fails.
exec 9>"$lock"
if ! flock --exclusive --nonblock 9; then
    exit 75
fi
printf 'ready\n'
cat >/dev/null
