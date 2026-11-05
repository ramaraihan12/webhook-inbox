#!/bin/sh
# Starts the demo in one container: the sink on loopback, then the inbox.

cgroup() { cat "/sys/fs/cgroup/$1" 2>/dev/null || echo unknown; }

cleanup() {
  PGCONNECT_TIMEOUT=10 psql "$DATABASE_URL" -qX -v ON_ERROR_STOP=1 -f /etc/webhook-inbox/cleanup.sql \
    || echo "cleanup failed, trying again in 10 minutes"
}

echo "limits: nproc=$(nproc) cpu.max=$(cgroup cpu.max) memory.max=$(cgroup memory.max)"

# Render sets PORT for the public port. The sink must not take it.
PORT=8021 SINK_HOST=127.0.0.1 echo-sink &

# Before the inbox, so the first page after a sleep has nothing stale on it. It
# also wakes Neon, so the inbox's 5 second connect timeout does not have to.
cleanup
(
  while sleep 600; do
    echo "memory: current=$(cgroup memory.current) peak=$(cgroup memory.peak)"
    cleanup
  done
) &

# exec, so the inbox is PID 1 and gets the SIGTERM for its graceful shutdown.
exec webhook-inbox serve
