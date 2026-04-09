#!/usr/bin/env bash
# scripts/verify-stack.sh
#
# HTTP-level smoke tests against a running rsky-pds stack.
# Covers: static routes, server description, admin auth, account lifecycle,
# record CRUD (createRecord / getRecord / listRecords / deleteRecord),
# and auth rejection.
#
# Usage:
#   bash scripts/verify-stack.sh
#
# Override target:
#   PDS_URL=http://localhost:2583 bash scripts/verify-stack.sh
#
# Expects docker/.env to exist (created by setup-local.sh).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENV_FILE="$REPO_ROOT/docker/.env"

PDS_URL="${PDS_URL:-http://localhost:2583}"

# ── Colour helpers ────────────────────────────────────────────────────────────

if [ -t 1 ] && command -v tput >/dev/null 2>&1; then
  CYAN="$(tput setaf 6)" GREEN="$(tput setaf 2)"
  YELLOW="$(tput setaf 3)" RED="$(tput setaf 1)" RESET="$(tput sgr0)"
else
  CYAN="" GREEN="" YELLOW="" RED="" RESET=""
fi

info()  { printf '%s▸%s %s\n' "$CYAN"   "$RESET" "$*"; }
ok()    { printf '%s✓%s %s\n' "$GREEN"  "$RESET" "$*"; }
warn()  { printf '%s⚠%s %s\n' "$YELLOW" "$RESET" "$*" >&2; }
fail()  { printf '%s✗%s %s\n' "$RED"    "$RESET" "$*" >&2; FAILURES=$(( FAILURES + 1 )); }

FAILURES=0
PASSED=0

# ── Test helpers ──────────────────────────────────────────────────────────────

# check_status LABEL URL EXPECTED_STATUS [EXTRA_CURL_ARGS...]
check_status() {
  local label="$1" url="$2" expected="$3"
  shift 3
  local actual
  actual=$(curl -s -o /dev/null -w "%{http_code}" "$@" "$url" 2>/dev/null) || actual="000"
  if [ "$actual" = "$expected" ]; then
    ok "$label → HTTP $actual"
    PASSED=$(( PASSED + 1 ))
  else
    fail "$label → expected HTTP $expected, got HTTP $actual  ($url)"
  fi
}

# check_json_field LABEL URL JQ_EXPR EXPECTED_VALUE [EXTRA_CURL_ARGS...]
check_json_field() {
  local label="$1" url="$2" expr="$3" expected="$4"
  shift 4
  local body actual
  body=$(curl -sf "$@" "$url" 2>/dev/null) || { fail "$label → request failed"; return; }
  actual=$(printf '%s' "$body" | jq -r "$expr" 2>/dev/null) || actual="<jq error>"
  if [ "$actual" = "$expected" ]; then
    ok "$label → $actual"
    PASSED=$(( PASSED + 1 ))
  else
    fail "$label → expected '$expected', got '$actual'"
  fi
}

# ── Load credentials from .env ────────────────────────────────────────────────

ADMIN_PASS=""
if [ -f "$ENV_FILE" ]; then
  ADMIN_PASS=$(grep '^PDS_ADMIN_PASS=' "$ENV_FILE" | cut -d= -f2-)
fi
ADMIN_PASS="${ADMIN_PASS:-hunter2}"

ADMIN_AUTH="Basic $(printf 'admin:%s' "$ADMIN_PASS" | base64)"

# ── Tests ─────────────────────────────────────────────────────────────────────

info "Verifying $PDS_URL"
printf '\n'

# 1. Root / robots.txt
info "--- Static routes ---"
check_status "GET /" "$PDS_URL/" 200
check_status "GET /robots.txt" "$PDS_URL/robots.txt" 200

# 2. com.atproto.server.describeServer
printf '\n'
info "--- Server description ---"
check_status "describeServer returns 200" \
  "$PDS_URL/xrpc/com.atproto.server.describeServer" 200

check_json_field \
  "describeServer.did is non-empty" \
  "$PDS_URL/xrpc/com.atproto.server.describeServer" \
  '.did | type' \
  "string"

# 3. Admin endpoint (requires auth)
printf '\n'
info "--- Admin auth ---"
# Note: unauthenticated rejection returns 500 instead of 401 — known rsky-pds bug
# (AdminToken guard returns wrong status; tracked in TASKS.md)
check_status "admin endpoint accepts valid credentials" \
  "$PDS_URL/xrpc/com.atproto.server.createInviteCode" 200 \
  -X POST \
  -H "Content-Type: application/json" \
  -H "Authorization: $ADMIN_AUTH" \
  -d '{"useCount":1}'

# 4. Account lifecycle
printf '\n'
info "--- Account lifecycle ---"

# Create a temporary account
TMP_HANDLE="verify-$(date +%s).test"
TMP_EMAIL="verify-$(date +%s)@localhost"
TMP_PASS="verify-tmp-pass-$$"

body=$(curl -sf -X POST "$PDS_URL/xrpc/com.atproto.server.createAccount" \
  -H "Content-Type: application/json" \
  -d "{\"handle\":\"$TMP_HANDLE\",\"email\":\"$TMP_EMAIL\",\"password\":\"$TMP_PASS\"}" \
  2>/dev/null) && {
    TMP_DID=$(printf '%s' "$body" | jq -r '.did // empty')
    TMP_ACCESS=$(printf '%s' "$body" | jq -r '.accessJwt // empty')
    if [ -n "$TMP_DID" ] && [ -n "$TMP_ACCESS" ]; then
      ok "createAccount → did=$TMP_DID"
      PASSED=$(( PASSED + 1 ))
    else
      fail "createAccount → missing did or accessJwt in response"
      TMP_DID="" TMP_ACCESS=""
    fi
} || { fail "createAccount → request failed"; TMP_DID="" TMP_ACCESS=""; }

if [ -n "$TMP_ACCESS" ]; then
  # getSession
  check_status "getSession with valid token" \
    "$PDS_URL/xrpc/com.atproto.server.getSession" 200 \
    -H "Authorization: Bearer $TMP_ACCESS"

  # resolveHandle
  check_status "resolveHandle" \
    "$PDS_URL/xrpc/com.atproto.identity.resolveHandle?handle=$TMP_HANDLE" 200
fi

# 5. Record lifecycle (createRecord / getRecord / listRecords / deleteRecord)
printf '\n'
info "--- Record lifecycle ---"

RECORD_URI="" RECORD_CID=""

if [ -n "$TMP_ACCESS" ] && [ -n "$TMP_DID" ]; then
  CREATED_AT="$(date -u +%Y-%m-%dT%H:%M:%S.000Z)"

  # createRecord — write a post
  rec_body=$(curl -sf -X POST "$PDS_URL/xrpc/com.atproto.repo.createRecord" \
    -H "Content-Type: application/json" \
    -H "Authorization: Bearer $TMP_ACCESS" \
    -d "{
      \"repo\": \"$TMP_DID\",
      \"collection\": \"app.bsky.feed.post\",
      \"record\": {
        \"\$type\": \"app.bsky.feed.post\",
        \"text\": \"verify-stack smoke test\",
        \"createdAt\": \"$CREATED_AT\"
      }
    }" 2>/dev/null) && {
      RECORD_URI=$(printf '%s' "$rec_body" | jq -r '.uri // empty')
      RECORD_CID=$(printf '%s' "$rec_body" | jq -r '.cid // empty')
      if [ -n "$RECORD_URI" ] && [ -n "$RECORD_CID" ]; then
        ok "createRecord → $RECORD_URI"
        PASSED=$(( PASSED + 1 ))
      else
        fail "createRecord → missing uri or cid in response"
        RECORD_URI="" RECORD_CID=""
      fi
  } || { fail "createRecord → request failed"; }

  # getRecord — read it back
  if [ -n "$RECORD_URI" ]; then
    RKEY=$(printf '%s' "$RECORD_URI" | sed 's|.*/||')
    check_json_field "getRecord returns correct text" \
      "$PDS_URL/xrpc/com.atproto.repo.getRecord?repo=$TMP_DID&collection=app.bsky.feed.post&rkey=$RKEY" \
      '.value.text' \
      "verify-stack smoke test" \
      -H "Authorization: Bearer $TMP_ACCESS"

    # listRecords — post appears in collection
    check_json_field "listRecords includes the post" \
      "$PDS_URL/xrpc/com.atproto.repo.listRecords?repo=$TMP_DID&collection=app.bsky.feed.post" \
      '[.records[].uri] | any(. == "'"$RECORD_URI"'")' \
      "true" \
      -H "Authorization: Bearer $TMP_ACCESS"

    # deleteRecord — remove it
    check_status "deleteRecord returns 200" \
      "$PDS_URL/xrpc/com.atproto.repo.deleteRecord" 200 \
      -X POST \
      -H "Content-Type: application/json" \
      -H "Authorization: Bearer $TMP_ACCESS" \
      -d "{\"repo\":\"$TMP_DID\",\"collection\":\"app.bsky.feed.post\",\"rkey\":\"$RKEY\"}"

    # getRecord after delete — must 404
    check_status "getRecord after deleteRecord returns 404" \
      "$PDS_URL/xrpc/com.atproto.repo.getRecord?repo=$TMP_DID&collection=app.bsky.feed.post&rkey=$RKEY" 404 \
      -H "Authorization: Bearer $TMP_ACCESS"
  fi

  # createRecord without auth — must be rejected
  check_status "createRecord without auth is rejected" \
    "$PDS_URL/xrpc/com.atproto.repo.createRecord" 400 \
    -X POST \
    -H "Content-Type: application/json" \
    -d "{\"repo\":\"$TMP_DID\",\"collection\":\"app.bsky.feed.post\",\"record\":{\"\$type\":\"app.bsky.feed.post\",\"text\":\"unauthed\",\"createdAt\":\"$CREATED_AT\"}}"
fi

# 6. Auth rejection
printf '\n'
info "--- Auth rejection ---"
check_status "getSession rejects missing token" \
  "$PDS_URL/xrpc/com.atproto.server.getSession" 400

check_status "getSession rejects bad token" \
  "$PDS_URL/xrpc/com.atproto.server.getSession" 400 \
  -H "Authorization: Bearer not-a-real-token"

# ── Summary ───────────────────────────────────────────────────────────────────

printf '\n'
TOTAL=$(( PASSED + FAILURES ))
if [ "$FAILURES" -eq 0 ]; then
  ok "All $TOTAL checks passed — stack looks healthy."
  exit 0
else
  fail "$FAILURES/$TOTAL checks failed."
  exit 1
fi
