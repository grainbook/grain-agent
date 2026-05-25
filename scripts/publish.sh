#!/usr/bin/env bash
set -euo pipefail

# ── publish.sh ──────────────────────────────────────────────────
# Publish all grain-ai workspace crates to crates.io in dependency
# order.  Safe: each crate is published only if its version doesn't
# already exist on crates.io (idempotent).
#
# Usage:
#   ./scripts/publish.sh                # prompt before each crate
#   ./scripts/publish.sh --yes          # skip confirmation
#   ./scripts/publish.sh --dry-run      # cargo publish --dry-run for each
#   ./scripts/publish.sh --skip-published  # auto-skip crates already on crates.io
#
# Requirements:
#   - `cargo login` already done (CARGO_REGISTRY_TOKEN set)
#   - All changes committed and pushed (clean working tree recommended)
# ──────────────────────────────────────────────────────────────────


RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

DRY_RUN=false
SKIP_CONFIRM=false
SKIP_PUBLISHED=false

for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=true ;;
    --yes|-y)  SKIP_CONFIRM=true ;;
    --skip-published) SKIP_PUBLISHED=true ;;
    *)
      echo "Unknown flag: $arg"
      echo "Usage: $0 [--dry-run] [--yes] [--skip-published]"
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

echo -e "${CYAN}── grain-ai monorepo publisher ──${NC}"
echo "Crates to publish (${#CRATES[@]}):"
for c in "${CRATES[@]}"; do
  printf "  %-32s  %s\n" "$c" "$(version_of "$c")"
done
echo ""

# ── Main loop ────────────────────────────────────────────────────
FAILED=()
for crate in "${CRATES[@]}"; do
  ver=$(version_of "$crate")
  echo -e "${YELLOW}▶ ${crate} v${ver}${NC}"

  if $SKIP_PUBLISHED && published_to_crates_io "$crate" "$ver"; then
    echo -e "  ${GREEN}✓ already published — skip${NC}"
    continue
  fi

  if ! $DRY_RUN && ! $SKIP_CONFIRM; then
    read -r -p "  Publish ${crate} v${ver}? [y/N] " yn
    case "$yn" in
      [yY]*) ;;
      *) echo "  Skipped."; FAILED+=("$crate (user skipped)"); continue ;;
    esac
  fi

  if $DRY_RUN; then
    cargo publish -p "$crate" --dry-run --allow-dirty
  else
    if cargo publish -p "$crate" --allow-dirty; then
      echo -e "  ${GREEN}✓ published${NC}"
    else
      echo -e "  ${RED}✗ FAILED${NC}"
      FAILED+=("$crate ($?)")
    fi
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
