#!/usr/bin/env bash
# Builds everything and runs the demo: the agentdb server on port 4000 and the
# Next.js app on port 3000. Settings come from .env (see README).
#
# The app listens on every interface so that a proxy in front of the machine
# can reach it, and lets nobody in without the demo password.
set -euo pipefail
cd "$(dirname "$0")"
export PATH="$HOME/.cargo/bin:$PATH"
set -a; . ./.env; set +a

cargo build --release --bin agentdb-server
(cd sdk && npm install --silent && npm run --silent build)
(cd demo && npm install --silent && npx next build)

export AGENTDB_PORT="${AGENTDB_PORT:-4000}"
export AGENTDB_URL="http://127.0.0.1:$AGENTDB_PORT"
if [ -z "${DEMO_PASSWORD:-}" ]; then
  DEMO_PASSWORD="$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')"
  echo "Demo password for this run: $DEMO_PASSWORD (set DEMO_PASSWORD in .env to keep one)"
fi
export DEMO_PASSWORD
./target/release/agentdb-server &
server=$!
trap 'kill $server 2>/dev/null' EXIT
cd demo && npx next start -H 0.0.0.0 -p "${PORT:-3000}"
