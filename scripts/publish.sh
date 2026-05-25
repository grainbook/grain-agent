#!/usr/bin/env bash
set -euo pipefail

# ── publish.sh ──────────────────────────────────────────────────
# Publish all grain-ai workspace crates to crates.io in dependency
# order.
#
# Idempotent and rate-limit aware:
#   - skips crates whose current version is already on crates.io
#   - on HTTP 429 ("too many new crates"), parses crates.io's
#     `try again after <RFC1123>` and sleeps until then, then retries
#   - after each successful publish, waits for the crates.io index to
#     reflect the new version before continuing (so dependent crates
#     can resolve it)
#   - between *brand-new* crates, sleeps NEW_CRATE_DELAY seconds
#     (default 605) to stay under the new-crate rate limit
#     (see https://crates.io/docs/rate-limits)
#
# Usage:
#   ./scripts/publish.sh                # prompt before each crate
#   ./scripts/publish.sh --yes          # skip confirmation
#   ./scripts/publish.sh --dry-run      # cargo publish --dry-run for each
#   ./scripts/publish.sh --no-skip      # don't auto-skip published crates
#   ./scripts/publish.sh --no-throttle  # skip preventive new-crate delay
#
# Env:
#   NEW_CRATE_DELAY   seconds to wait between new-crate publishes
#                     (default 605 ≈ 10m + 5s buffer)
#   INDEX_WAIT_SECS   max seconds to wait for index propagation (default 600)
#
# Requirements:
#   - `cargo login` already done (CARGO_REGISTRY_TOKEN set)
# ──────────────────────────────────────────────────────────────────

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

DRY_RUN=false
SKIP_CONFIRM=false
SKIP_PUBLISHED=true   # default ON: this script is meant to be resumable
THROTTLE=true

NEW_CRATE_DELAY="${NEW_CRATE_DELAY:-605}"
INDEX_WAIT_SECS="${INDEX_WAIT_SECS:-600}"

for arg in "$@"; do
  case "$arg" in
    --dry-run)        DRY_RUN=true ;;
    --yes|-y)         SKIP_CONFIRM=true ;;
    --skip-published) SKIP_PUBLISHED=true ;;
    --no-skip)        SKIP_PUBLISHED=false ;;
    --no-throttle)    THROTTLE=false ;;
    *)
      echo "Unknown flag: $arg"
      echo "Usage: $0 [--dry-run] [--yes] [--no-skip] [--no-throttle]"
      exit 1
      ;;
  esac
done

# ── Publish order (topological sort by dependency graph) ───────
CRATES=(
  grain-agent-core
  grain-llm-models
  grain-deepseek-pack
  grain-llm-genai
  grain-script-boa
  grain-script-rhai
  grain-plugin-wasm
  grain-agent-harness
  grain-ai-agent-headless
  grain-ai-agent-tui
)

# ── Helpers ──────────────────────────────────────────────────────
version_of() {
  grep '^version' "$1/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/'
}

published_to_crates_io() {
  local name="$1" ver="$2"
  local code
  code=$(curl -s -o /dev/null -w '%{http_code}' \
    "https://crates.io/api/v1/crates/${name}/${ver}" 2>/dev/null || true)
  [[ "$code" == "200" ]]
}

# Parse RFC1123 date string into epoch seconds. Tries GNU then BSD form.
parse_http_date() {
  local s="$1" out
  out=$(date -u -d "$s" +%s 2>/dev/null || true)
  if [[ -z "$out" ]]; then
    out=$(date -j -u -f "%a, %d %b %Y %H:%M:%S %Z" "$s" +%s 2>/dev/null || true)
  fi
  printf '%s' "$out"
}

# Extract "try again after <RFC1123>" from cargo's stderr and sleep
# until that wall-clock time. Returns 0 if it slept, 1 if no match.
sleep_until_retry_after() {
  local err="$1" after_str target now wait
  after_str=$(printf '%s' "$err" \
    | grep -Eo 'try again after [^.]+(GMT|UTC)' \
    | head -1 \
    | sed 's/^try again after //')
  [[ -z "$after_str" ]] && return 1
  target=$(parse_http_date "$after_str")
  [[ -z "$target" ]] && return 1
  now=$(date -u +%s)
  wait=$(( target - now + 10 ))
  (( wait < 10 )) && wait=10
  echo -e "  ${YELLOW}⏳ 429 rate-limited; sleeping ${wait}s (until ${after_str})${NC}"
  sleep "$wait"
  return 0
}

# Poll crates.io API until name@ver is observable, or timeout.
wait_for_index() {
  local name="$1" ver="$2"
  local deadline=$(( $(date -u +%s) + INDEX_WAIT_SECS ))
  local first=true
  while ! published_to_crates_io "$name" "$ver"; do
    if (( $(date -u +%s) >= deadline )); then
      echo -e "  ${RED}✗ index never reflected ${name}@${ver} in ${INDEX_WAIT_SECS}s${NC}"
      return 1
    fi
    if $first; then
      echo -e "  ${CYAN}…waiting for crates.io index${NC}"
      first=false
    fi
    sleep 5
  done
  echo -e "  ${GREEN}✓ index observes ${name}@${ver}${NC}"
}

# Publish one crate with 429 retry. Captures combined output via PIPESTATUS.
# Returns 0 on success, 1 on terminal failure.
publish_one() {
  local crate="$1"
  local tmp attempt=0 status
  tmp=$(mktemp)
  while :; do
    attempt=$(( attempt + 1 ))
    set +e
    cargo publish -p "$crate" --allow-dirty 2>&1 | tee "$tmp"
    status=${PIPESTATUS[0]}
    set -e
    if (( status == 0 )); then
      rm -f "$tmp"
      return 0
    fi
    if (( attempt < 3 )) && grep -qE '429|Too Many Requests|too many new crates' "$tmp"; then
      if sleep_until_retry_after "$(cat "$tmp")"; then
        : > "$tmp"
        continue
      fi
    fi
    rm -f "$tmp"
    return 1
  done
}

echo -e "${CYAN}── grain-ai monorepo publisher ──${NC}"
echo "Crates to publish (${#CRATES[@]}):"
for c in "${CRATES[@]}"; do
  printf "  %-32s  %s\n" "$c" "$(version_of "$c")"
done
echo ""

# ── Main loop ────────────────────────────────────────────────────
FAILED=()
PUBLISHED_NEW_THIS_RUN=0
for crate in "${CRATES[@]}"; do
  ver=$(version_of "$crate")
  echo -e "${YELLOW}▶ ${crate} v${ver}${NC}"

  if $SKIP_PUBLISHED && published_to_crates_io "$crate" "$ver"; then
    echo -e "  ${GREEN}✓ already published — skip${NC}"
    echo ""
    continue
  fi

  if ! $DRY_RUN && ! $SKIP_CONFIRM; then
    read -r -p "  Publish ${crate} v${ver}? [y/N] " yn
    case "$yn" in
      [yY]*) ;;
      *) echo "  Skipped."; FAILED+=("$crate (user skipped)"); echo ""; continue ;;
    esac
  fi

  if $DRY_RUN; then
    if cargo publish -p "$crate" --dry-run --allow-dirty; then
      echo -e "  ${GREEN}✓ dry-run ok${NC}"
    else
      FAILED+=("$crate (dry-run)")
    fi
    echo ""
    continue
  fi

  # Throttle BEFORE publishing the next brand-new crate (after the first
  # one in this run) to stay under crates.io's new-crate rate limit.
  if $THROTTLE && (( PUBLISHED_NEW_THIS_RUN >= 1 )); then
    echo -e "  ${YELLOW}⏳ new-crate throttle: sleeping ${NEW_CRATE_DELAY}s${NC}"
    sleep "$NEW_CRATE_DELAY"
  fi

  if publish_one "$crate"; then
    echo -e "  ${GREEN}✓ published${NC}"
    PUBLISHED_NEW_THIS_RUN=$(( PUBLISHED_NEW_THIS_RUN + 1 ))
    if ! wait_for_index "$crate" "$ver"; then
      FAILED+=("$crate (index never updated)")
    fi
  else
    echo -e "  ${RED}✗ FAILED${NC}"
    FAILED+=("$crate")
    # Don't continue past a failed dependency — later crates will just
    # error with "no matching package". Bail early.
    echo -e "${RED}── aborting: ${crate} failed; later crates depend on it ──${NC}"
    break
  fi
  echo ""
done

if [[ ${#FAILED[@]} -gt 0 ]]; then
  echo -e "${RED}── ${#FAILED[@]} crate(s) failed ──${NC}"
  for f in "${FAILED[@]}"; do
    echo "  ✗ $f"
  done
  exit 1
fi

$DRY_RUN && echo -e "${GREEN}── Dry-run complete (no crates published) ──${NC}" && exit 0
echo -e "${GREEN}── All ${#CRATES[@]} crates published ──${NC}"
