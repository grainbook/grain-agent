#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Grain Agent — one-command installer.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/grain-ai/grain-agent/main/install.sh | bash
#   # or locally:
#   ./install.sh
#
# Installs `grain-tui` and/or `grain-headless` into
# `~/.cargo/bin/` (or `$CARGO_HOME/bin`).  Requires `cargo` on PATH.
#
# Options (environment variables):
#   COMPONENTS  Space-separated list of components to install
#               (default: "tui headless").  Each component maps to a
#               crate: tui → grain-ai-agent-tui, headless → grain-ai-agent-headless.
#   FEATURES    Cargo `--features` (default: unset → crate defaults).
#   NO_DEFAULT_FEATURES  If=1, pass `--no-default-features`.
#   REF         Git branch/tag/rev (default: "main").
#   REPO        Git URL (default: "https://github.com/grain-ai/grain-agent").
#   INSTALL_DIR Destination dir (default: "$CARGO_HOME/bin").
#
#   GRAIN_SKIP_CONFIRM  If=1, skip the confirmation prompt.
#   GRAIN_DRY_RUN       If=1, print what would be done and exit.
# ---------------------------------------------------------------------------
set -euo pipefail

# --- config ---------------------------------------------------------------
REPO="${REPO:-https://github.com/grain-ai/grain-agent}"
REF="${REF:-main}"
COMPONENTS="${COMPONENTS:-tui headless}"
FEATURES="${FEATURES:-}"
NO_DEFAULT_FEATURES="${NO_DEFAULT_FEATURES:-}"
INSTALL_DIR="${INSTALL_DIR:-${CARGO_HOME:-$HOME/.cargo}/bin}"
GRAIN_SKIP_CONFIRM="${GRAIN_SKIP_CONFIRM:-}"
GRAIN_DRY_RUN="${GRAIN_DRY_RUN:-}"

# Map component short-names → crate name + installed binary name.
# "tui" installs the `grain-ai-agent-tui` crate → binary `grain-tui`.
# "headless" installs `grain-ai-agent-headless` → binary `grain-headless`.
declare -A CRATE_OF=()
CRATE_OF["tui"]="grain-ai-agent-tui"
CRATE_OF["headless"]="grain-ai-agent-headless"
declare -A BIN_OF=()
BIN_OF["tui"]="grain-tui"
BIN_OF["headless"]="grain-headless"

# --- helpers --------------------------------------------------------------
GREEN='\033[0;32m'
CYAN='\033[0;36m'
BOLD='\033[1m'
RED='\033[0;31m'
NC='\033[0m'

say()   { printf "${GREEN}→${NC} %s\n" "$*"; }
warn()  { printf "${RED}⚠${NC} %s\n" "$*" >&2; }
info()  { printf "  %s\n" "$*"; }
header() {
  echo ""
  printf "${BOLD}${CYAN}╔══════════════════════════════════════╗${NC}\n"
  printf "${BOLD}${CYAN}║   Grain Agent Installer              ║${NC}\n"
  printf "${BOLD}${CYAN}╚══════════════════════════════════════╝${NC}\n"
  echo ""
}

# --- main ------------------------------------------------------------------
main() {
  header

  # ---- check rust --------------------------------------------------------
  if ! command -v cargo >/dev/null 2>&1; then
    warn "Rust is not installed (cargo not found on PATH)."
    printf "\n${BOLD}Install Rust via rustup:${NC}\n"
    printf "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh\n"
    printf "\nRestart your shell and re-run this script.\n"
    exit 1
  fi
  say "cargo $(cargo --version | cut -d' ' -f2)"
  say "rustc $(rustc --version | cut -d' ' -f2)"

  # ---- target dir --------------------------------------------------------
  if [ ! -d "$INSTALL_DIR" ]; then
    mkdir -p "$INSTALL_DIR"
    say "created $INSTALL_DIR"
  fi

  # ---- feature flags -----------------------------------------------------
  local feat_arg=()
  if [ "${NO_DEFAULT_FEATURES}" = "1" ]; then
    feat_arg+=("--no-default-features")
  fi
  if [ -n "${FEATURES}" ]; then
    feat_arg+=("--features" "${FEATURES}")
  fi

  # ---- summary -----------------------------------------------------------
  info "Repository:   ${REPO}"
  info "Branch/tag:   ${REF}"
  info "Components:   ${COMPONENTS}"
  if [ ${#feat_arg[@]} -gt 0 ]; then
    info "Features:     ${feat_arg[*]}"
  else
    info "Features:     (crate defaults)"
  fi
  info "Install dir:  ${INSTALL_DIR}"

  # Show which binaries will be installed
  local bins=()
  for comp in ${COMPONENTS}; do
    if [ -n "${BIN_OF[$comp]:-}" ]; then
      bins+=("${BIN_OF[$comp]}")
    fi
  done
  info "Will install: ${bins[*]}"
  echo ""

  # ---- confirm -----------------------------------------------------------
  if [ "${GRAIN_SKIP_CONFIRM}" != "1" ] && [ "${GRAIN_DRY_RUN}" != "1" ]; then
    printf "${CYAN}Proceed with install? [Y/n] ${NC}"
    read -r answer
    case "${answer:-y}" in
      [Yy]*) ;;
      *)     info "aborted."; exit 0 ;;
    esac
  fi

  if [ "${GRAIN_DRY_RUN}" = "1" ]; then
    info "dry-run — would run:"
    for comp in ${COMPONENTS}; do
      local crate="${CRATE_OF[$comp]:-}"
      if [ -z "${crate}" ]; then
        warn "unknown component: ${comp} (expected: tui, headless)"
        continue
      fi
      info "  cargo install --git ${REPO} --branch ${REF} ${feat_arg[*]} --locked ${crate}"
    done
    exit 0
  fi

  # ---- install -----------------------------------------------------------
  local failed=0
  for comp in ${COMPONENTS}; do
    local crate="${CRATE_OF[$comp]:-}"
    local bin="${BIN_OF[$comp]:-}"

    if [ -z "${crate}" ]; then
      warn "unknown component: ${comp} — skipping (expected: tui, headless)"
      failed=1
      continue
    fi

    echo ""
    say "installing ${bin} (crate: ${crate}) …"

    # --locked ensures reproducible builds from the repo's Cargo.lock.
    # We filter build output to keep it readable: show only the
    # final status line + any warnings/errors.
    cargo install \
      --git "${REPO}" \
      --branch "${REF}" \
      ${feat_arg[@]+"${feat_arg[@]}"} \
      --locked \
      "${crate}" 2>&1 | while IFS= read -r line; do
        case "${line}" in
          *Installed*|*warning*|*error*:*|*Error:*|*Finished*)
            printf "  %s\n" "${line}" ;;
          *) : ;;
        esac
      done

    if [ -x "${INSTALL_DIR}/${bin}" ]; then
      say "${bin}  ${GREEN}✓${NC}"
    else
      warn "${bin} install may have failed — binary not found at ${INSTALL_DIR}/${bin}"
      failed=1
    fi
  done

  # ---- post-install ------------------------------------------------------
  echo ""
  if [ "${failed}" -eq 0 ]; then
    printf "${BOLD}${GREEN}Done!${NC}\n"
  else
    printf "${BOLD}${RED}Done with errors.${NC}\n"
  fi
  echo ""
  if ! echo "${PATH}" | grep -qF "${INSTALL_DIR}"; then
    warn "${INSTALL_DIR} is not on your PATH."
    printf "  Add this to your shell rc file (~/.bashrc, ~/.zshrc):\n"
    printf "  ${CYAN}export PATH=\"\${CARGO_HOME:-\$HOME/.cargo}/bin:\$PATH\"${NC}\n"
    echo ""
  fi
  info "Get started:  grain-tui --help"
  info "              grain-headless --help"
  info "Docs:         https://github.com/grain-ai/grain-agent#readme"
  echo ""
  exit "${failed}"
}

main "$@"
