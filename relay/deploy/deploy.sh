#!/usr/bin/env bash
# Deploy the Wavefront relay to a VPS over SSH (key auth).
#
#   ./deploy.sh root@your.vps.ip
#
# Idempotent: syncs the relay source + webclient, builds release on the box,
# installs the binary + systemd unit, (re)starts the service. First run also
# installs Rust and opens the firewall port. Uses your SSH key — no passwords.
set -euo pipefail

HOST="${1:?usage: ./deploy.sh user@host}"
REMOTE_DIR=/opt/wavefront-relay
PORT=8927
HERE="$(cd "$(dirname "$0")/../.." && pwd)"   # repo root

echo "==> Syncing sources to $HOST"
ssh "$HOST" "mkdir -p $REMOTE_DIR/build/relay"
rsync -az --exclude target "$HERE/relay/" "$HOST:$REMOTE_DIR/build/relay/"
rsync -az "$HERE/webclient/" "$HOST:$REMOTE_DIR/webclient/"
scp -q "$HERE/relay/deploy/wavefront-relay.service" "$HOST:/etc/systemd/system/wavefront-relay.service"

echo "==> Building + installing on $HOST"
ssh "$HOST" bash -s -- "$REMOTE_DIR" "$PORT" <<'REMOTE'
set -euo pipefail
REMOTE_DIR="$1"; PORT="$2"

# Rust (first run only)
if ! command -v cargo >/dev/null 2>&1 && [ ! -x "$HOME/.cargo/bin/cargo" ]; then
  echo "  installing rust..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y -q
fi
source "$HOME/.cargo/env"

# Firewall (best-effort)
command -v ufw >/dev/null 2>&1 && ufw allow "$PORT"/tcp >/dev/null 2>&1 || true

cd "$REMOTE_DIR/build/relay"
cargo build --release
systemctl stop wavefront-relay 2>/dev/null || true
cp target/release/wavefront-relay "$REMOTE_DIR/wavefront-relay"
systemctl daemon-reload
systemctl enable --now wavefront-relay
sleep 1
systemctl is-active wavefront-relay
curl -s -o /dev/null -w "health: HTTP %{http_code}\n" "http://127.0.0.1:$PORT/healthz"
REMOTE

echo "==> Done. Relay live on port $PORT."
echo "    Speakers open:  http://<vps-ip>:$PORT/"
echo "    Browser host:   http://<vps-ip>:$PORT/host.html   (needs HTTPS + Chromium)"
