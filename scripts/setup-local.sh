#!/usr/bin/env bash
# scripts/setup-local.sh
#
# One-shot setup for the local rsky-pds development stack.
# Run this once after cloning, or whenever you want a fresh environment.
#
# Prerequisites:
#   - A container runtime with compose support (see below)
#   - openssl, curl, jq in PATH
#   - cargo + diesel-cli for migrations (diesel-cli is installed automatically if absent)
#   - On macOS: brew install libpq  (required for diesel-cli linking)
#
# Supported container runtimes (auto-detected in order):
#   - Docker Desktop / Colima / Rancher Desktop (moby):  docker compose  [plugin]
#   - Legacy Docker:                                      docker-compose  [standalone]
#   - Rancher Desktop (containerd):                       nerdctl compose
#   - Podman:                                             podman compose
#     Podman requires the socket to be running:
#       podman machine start   (macOS/Windows)
#       systemctl --user start podman.socket   (Linux)
#     Then export DOCKER_HOST="unix://$(podman machine inspect --format '{{.ConnectionInfo.PodmanSocket.Path}}')"
#
#   Override auto-detection: COMPOSE_CMD="nerdctl compose" bash scripts/setup-local.sh
#
# Usage:
#   bash scripts/setup-local.sh
#
# Override smoke-test credentials via environment:
#   SMOKE_ACCOUNT1_HANDLE=alice.test SMOKE_ACCOUNT1_PASS=secret1 \
#   SMOKE_ACCOUNT2_HANDLE=bob.test   SMOKE_ACCOUNT2_PASS=secret2 \
#   bash scripts/setup-local.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DOCKER_DIR="$REPO_ROOT/docker"
ENV_FILE="$DOCKER_DIR/.env"
COMPOSE_FILE="$DOCKER_DIR/docker-compose.yml"

PDS_PORT="${PDS_PORT:-2583}"
PDS_URL="http://localhost:$PDS_PORT"

SMOKE_ACCOUNT1_HANDLE="${SMOKE_ACCOUNT1_HANDLE:-alice.test}"
SMOKE_ACCOUNT1_EMAIL="${SMOKE_ACCOUNT1_EMAIL:-alice@localhost}"
SMOKE_ACCOUNT1_PASS="${SMOKE_ACCOUNT1_PASS:-alice-local-pass}"

SMOKE_ACCOUNT2_HANDLE="${SMOKE_ACCOUNT2_HANDLE:-bob.test}"
SMOKE_ACCOUNT2_EMAIL="${SMOKE_ACCOUNT2_EMAIL:-bob@localhost}"
SMOKE_ACCOUNT2_PASS="${SMOKE_ACCOUNT2_PASS:-bob-local-pass}"

# ── Colour helpers (gracefully degrade when not a TTY) ────────────────────────

if [ -t 1 ] && command -v tput >/dev/null 2>&1; then
  CYAN="$(tput setaf 6)"
  GREEN="$(tput setaf 2)"
  YELLOW="$(tput setaf 3)"
  RED="$(tput setaf 1)"
  RESET="$(tput sgr0)"
else
  CYAN="" GREEN="" YELLOW="" RED="" RESET=""
fi

info()  { printf '%s▸%s %s\n'  "$CYAN"   "$RESET" "$*"; }
ok()    { printf '%s✓%s %s\n'  "$GREEN"  "$RESET" "$*"; }
warn()  { printf '%s⚠%s %s\n'  "$YELLOW" "$RESET" "$*" >&2; }
die()   { printf '%s✗%s %s\n'  "$RED"    "$RESET" "$*" >&2; exit 1; }

# ── Prerequisite checks ───────────────────────────────────────────────────────

for cmd in openssl curl jq; do
  command -v "$cmd" >/dev/null 2>&1 || die "Required command not found: $cmd"
done

# ── Detect compose command ────────────────────────────────────────────────────
# Allow explicit override via COMPOSE_CMD env var.
if [ -n "${COMPOSE_CMD:-}" ]; then
  COMPOSE_BIN="$COMPOSE_CMD"
elif docker compose version >/dev/null 2>&1; then
  COMPOSE_BIN="docker compose"
elif command -v docker-compose >/dev/null 2>&1; then
  COMPOSE_BIN="docker-compose"
elif nerdctl compose version >/dev/null 2>&1; then
  COMPOSE_BIN="nerdctl compose"
elif command -v podman >/dev/null 2>&1 && podman compose version >/dev/null 2>&1; then
  COMPOSE_BIN="podman compose"
else
  die "No compose runtime found. Install Docker Desktop, Rancher Desktop, Colima, or Podman with compose support."
fi
ok "Using compose: $COMPOSE_BIN"

# ── Helpers ───────────────────────────────────────────────────────────────────

gen_key() {
  # Generate a random 32-byte secp256k1-compatible private key (hex).
  # Probability of hitting an out-of-range key is ~2^-128 — acceptable for local dev.
  openssl rand -hex 32
}

wait_for_url() {
  local url="$1" label="$2" retries="${3:-30}" delay="${4:-2}"
  info "Waiting for $label..."
  local i=0
  while [ "$i" -lt "$retries" ]; do
    if curl -sf "$url" >/dev/null 2>&1; then
      ok "$label is ready"
      return 0
    fi
    sleep "$delay"
    i=$(( i + 1 ))
  done
  die "$label did not become ready after $(( retries * delay ))s"
}

compose() {
  $COMPOSE_BIN -f "$COMPOSE_FILE" --env-file "$ENV_FILE" "$@"
}

# ── Step 1: Write docker/.env ─────────────────────────────────────────────────

info "Generating environment file..."

if [ -f "$ENV_FILE" ]; then
  warn "$ENV_FILE already exists — skipping key generation (delete it to regenerate)"
else
  JWT_KEY=$(gen_key)
  PLC_KEY=$(gen_key)
  REPO_KEY=$(gen_key)

  sed \
    -e "s|^PDS_JWT_KEY_K256_PRIVATE_KEY_HEX=.*|PDS_JWT_KEY_K256_PRIVATE_KEY_HEX=$JWT_KEY|" \
    -e "s|^PDS_PLC_ROTATION_KEY_K256_PRIVATE_KEY_HEX=.*|PDS_PLC_ROTATION_KEY_K256_PRIVATE_KEY_HEX=$PLC_KEY|" \
    -e "s|^PDS_REPO_SIGNING_KEY_K256_PRIVATE_KEY_HEX=.*|PDS_REPO_SIGNING_KEY_K256_PRIVATE_KEY_HEX=$REPO_KEY|" \
    "$DOCKER_DIR/.env.example" > "$ENV_FILE"

  ok "Written $ENV_FILE (keys generated)"
fi

# ── Step 2: Start infrastructure (postgres + minio) ───────────────────────────

info "Starting infrastructure services..."
compose up -d postgres minio minio-setup
ok "Infrastructure started"

# ── Step 3: Run Diesel migrations ─────────────────────────────────────────────

info "Running database migrations..."

# Install diesel-cli if not present
if ! command -v diesel >/dev/null 2>&1; then
  info "diesel-cli not found — installing..."
  if [ "$(uname)" = "Darwin" ] && ! brew list libpq >/dev/null 2>&1; then
    die "libpq not installed. Run: brew install libpq"
  fi
  PQ_LIB_DIR="${PQ_LIB_DIR:-/opt/homebrew/opt/libpq/lib}" \
    cargo install diesel_cli --no-default-features --features postgres
fi

# Wait for postgres via pg_isready inside the container
info "Waiting for PostgreSQL to accept connections..."
pg_retries=30
while [ "$pg_retries" -gt 0 ]; do
  if $COMPOSE_BIN -f "$COMPOSE_FILE" --env-file "$ENV_FILE" \
      exec -T postgres pg_isready -U rsky -d rsky_pds >/dev/null 2>&1; then
    break
  fi
  sleep 2
  pg_retries=$(( pg_retries - 1 ))
done
[ "$pg_retries" -gt 0 ] || die "PostgreSQL did not become ready"
ok "PostgreSQL is ready"

# Load DATABASE_URL from .env for the host-facing connection
DB_URL=$(grep '^DATABASE_URL=' "$ENV_FILE" | cut -d= -f2-)
DB_URL="${DB_URL:-postgresql://rsky:rsky@localhost:5432/rsky_pds}"

PQ_LIB_DIR="${PQ_LIB_DIR:-/opt/homebrew/opt/libpq/lib}" \
  diesel migration run \
    --migration-dir "$REPO_ROOT/rsky-pds/migrations" \
    --database-url "$DB_URL"

ok "Migrations applied"

# ── Step 4: Start rsky-pds ────────────────────────────────────────────────────

info "Building and starting rsky-pds (first build may take several minutes)..."
compose up -d rsky-pds
wait_for_url "$PDS_URL/xrpc/com.atproto.server.describeServer" "rsky-pds" 60 5

# ── Step 5: Create test accounts ──────────────────────────────────────────────

create_account() {
  local handle="$1" email="$2" pass="$3"

  response=$(curl -sf -X POST "$PDS_URL/xrpc/com.atproto.server.createAccount" \
    -H "Content-Type: application/json" \
    -d "{\"handle\":\"$handle\",\"email\":\"$email\",\"password\":\"$pass\"}" \
    2>&1) || {
      warn "Account $handle may already exist or creation failed: $response"
      return 0
    }

  did=$(printf '%s' "$response" | jq -r '.did // empty')
  if [ -z "$did" ]; then
    warn "Could not parse DID for $handle — response: $response"
  else
    ok "Created account $handle (did=$did)"
  fi
}

info "Creating smoke-test accounts..."
create_account "$SMOKE_ACCOUNT1_HANDLE" "$SMOKE_ACCOUNT1_EMAIL" "$SMOKE_ACCOUNT1_PASS"
create_account "$SMOKE_ACCOUNT2_HANDLE" "$SMOKE_ACCOUNT2_EMAIL" "$SMOKE_ACCOUNT2_PASS"

# ── Step 6: Write atproto-smoke config ────────────────────────────────────────

SMOKE_CONFIG="$REPO_ROOT/atproto-smoke/config.json"
SMOKE_JSON=$(cat <<JSON
{
  "pdsUrl": "$PDS_URL",
  "account1": {
    "handle": "$SMOKE_ACCOUNT1_HANDLE",
    "password": "$SMOKE_ACCOUNT1_PASS"
  },
  "account2": {
    "handle": "$SMOKE_ACCOUNT2_HANDLE",
    "password": "$SMOKE_ACCOUNT2_PASS"
  }
}
JSON
)

if [ -d "$REPO_ROOT/atproto-smoke" ]; then
  info "Writing atproto-smoke/config.json..."
  printf '%s\n' "$SMOKE_JSON" > "$SMOKE_CONFIG"
  ok "Written $SMOKE_CONFIG"
else
  warn "atproto-smoke directory not found — skipping config.json"
  info "Clone it with: git clone https://github.com/aliceisjustplaying/atproto-smoke atproto-smoke"
  info "Then re-run this script, or create config.json manually:"
  printf '%s\n' "$SMOKE_JSON"
fi

# ── Done ──────────────────────────────────────────────────────────────────────

ADMIN_PASS=$(grep '^PDS_ADMIN_PASS=' "$ENV_FILE" | cut -d= -f2-)
MINIO_USER=$(grep '^AWS_ACCESS_KEY_ID=' "$ENV_FILE" | cut -d= -f2-)
MINIO_PASS=$(grep '^AWS_SECRET_ACCESS_KEY=' "$ENV_FILE" | cut -d= -f2-)

printf '\n'
ok "Stack is ready."
printf '  PDS:        %s\n'   "$PDS_URL"
printf '  MinIO UI:   %s  (%s / %s)\n' "http://localhost:9001" "${MINIO_USER:-minioadmin}" "${MINIO_PASS:-minioadmin}"
printf '  Admin:      user=admin  pass=%s\n' "${ADMIN_PASS:-hunter2}"
printf '\n'
printf 'Pre-flight (Rust integration tests, uses testcontainers — requires Docker):\n'
printf '  PDS_ADMIN_PASS=hunter2 PDS_JWT_KEY_K256_PRIVATE_KEY_HEX=$(openssl rand -hex 32) \\\n'
printf '  PDS_PLC_ROTATION_KEY_K256_PRIVATE_KEY_HEX=$(openssl rand -hex 32) \\\n'
printf '  PDS_REPO_SIGNING_KEY_K256_PRIVATE_KEY_HEX=$(openssl rand -hex 32) \\\n'
printf '  cargo test --release -p rsky-pds\n'
printf '\n'
printf 'Verify stack endpoints:\n'
printf '  bash scripts/verify-stack.sh\n'
printf '\n'
printf 'Run smoke tests:\n'
printf '  cd atproto-smoke && bun install && bun run test\n'
printf '\n'
printf 'Tear down:\n'
printf '  %s -f docker/docker-compose.yml down -v\n' "$COMPOSE_BIN"
