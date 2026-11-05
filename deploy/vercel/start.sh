#!/bin/sh
# Starts the demo in one container: the sink on loopback, then the inbox.

# Vercel sets PORT for the public port. The sink must not take it.
PORT=8021 SINK_HOST=127.0.0.1 echo-sink &

# Once per boot, never on a timer, so nothing here wakes Neon by itself: a boot
# only follows a real request and reaches the database anyway. In the background
# and a little later, because a visitor is waiting for this boot.
(
  sleep 5
  PGCONNECT_TIMEOUT=10 psql "$DATABASE_URL" -qX -v ON_ERROR_STOP=1 -f /etc/webhook-inbox/cleanup.sql \
    || echo "cleanup failed, the next boot tries again"
) &

# exec, so the inbox is PID 1 and gets the SIGTERM for its graceful shutdown.
exec webhook-inbox serve
