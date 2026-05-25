#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Grain Agent — one-command installer.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/grainbook/grain-agent/main/install.sh | bash
#   # or locally:
#   ./install.sh
#
# Installs `grain-tui` and/or `grain-headless` binaries to
# `~/.cargo/bin/` (or `$CARGO_HOME/bin`).
#
# **Default**: downloads prebuilt binaries from the latest GitHub release.
# Falls back to `cargo install --git` if no binary matches your platform.
#
# Options (CLI flags, take precedence over env vars):
#   --from-source        Force source build (cargo install --git)
#   --from-binary        Force binary download (fail if unavailable)
#   --tag <tag>          Specific release tag (default: latest)
#   --ref <ref>          Git ref for source builds (default: "main")
#
# Options (environment variables):
#   COMPONENTS           Space-separated list: "tui headless" (default: both)
#   REF                   Git branch/tag/rev for source builds (default: "main")
#   TAG                   Release tag for binary downloads (default: latest)
#   INSTALL_DIR           Destination dir (default: "$CARGO_HOME/bin")
#   GRAIN_SKIP_CONFIRM   If=1, skip the confirmation prompt
#   GRAIN_DRY_RUN         If=1, print what would be done and exit
# ---------------------------------------------------------------------------
set -euo pipefail

# ── config ────────────────────────────────────────────────────────────────
REPO="${REPO:-https://github.com/grainbook/grain-agent}"
REPO_OWNER="grainbook"
REPO_NAME="grain-agent"
REF="${REF:-main}"
TAG="${TAG:-latest}"
COMPONENTS="${COMPONENTS:-tui headless}"
INSTALL_DIR="${INSTALL_DIR:-${CARGO_HOME:-$HOME/.cargo}/bin}"
GRAIN_SKIP_CONFIRM="${GRAIN_SKIP_CONFIRM:-}"
GRAIN_DRY_RUN="${GRAIN_DRY_RUN:-}"

# Default: prefer binary, fall back to source.
INSTALL_MODE="auto"

# ── helpers (bash 3.2 compatible — no associative arrays) ─────────────────
crate_name() {
  case "$1" in
    tui)      echo "grain-ai-agent-tui" ;;
    headless) echo "grain-ai-agent-headless" ;;
    *)        echo "" ;;
  esac
}

bin_name() {
  case "$1" in
    tui)      echo "grain-tui" ;;
    headless) echo "grain-headless" ;;
    *)        echo "" ;;
  esac
}

valid_component() {
  case "$1" in
    tui|headless) return 0 ;;
    *)            return 1 ;;
  esac
}

# ── styling ───────────────────────────────────────────────────────────────
GREEN='\033[0;32m'
CYAN='\033[0;36m'
BOLD='\033[1m'
RED='\033[0;31m'
NC='\033[0m'

say() { printf "${GREEN}→${NC} %s\n" "$*"; }
warn() { printf "${RED}⚠${NC} %s\n" "$*" >&2; }
info() { printf "   %s\n" "$*"; }
header() {
  echo ""
  printf "${BOLD}${CYAN}╔══════════════════════════════════════╗${NC}\n"
  printf "${BOLD}${CYAN}║      Grain Agent Installer           ║${NC}\n"
  printf "${BOLD}${CYAN}╚══════════════════════════════════════╝${NC}\n"
  echo ""
}

# ── arg parsing ───────────────────────────────────────────────────────────
while [ $# -gt 0 ]; do
  case "$1" in
    --from-source) INSTALL_MODE="source"; shift ;;
    --from-binary) INSTALL_MODE="binary"; shift ;;
    --tag) TAG="$2"; shift 2 ;;
    --ref) REF="$2"; shift 2 ;;
    --help|-h)
      echo "Usage: $0 [--from-source|--from-binary] [--tag <tag>] [--ref <ref>]"
      echo ""
      echo "  --from-source   Force source build via cargo install"
      echo "  --from-binary   Force binary download from GitHub releases"
      echo "  --tag <tag>     Release tag for binary download (default: latest)"
      echo "  --ref <ref>     Git ref for source build (default: main)"
      exit 0
      ;;
    *)
      warn "Unknown option: $1"
      echo "Usage: $0 [--from-source|--from-binary] [--tag <tag>] [--ref <ref>]"
      exit 1
      ;;
  esac
done

# ── platform detection ────────────────────────────────────────────────────
detect_target() {
  local os=""
  local arch=""
  case "$(uname -s)" in
    Linux)  os="unknown-linux-gnu" ;;
    Darwin) os="apple-darwin" ;;
  esac
  case "$(uname -m)" in
    x86_64|amd64) arch="x86_64" ;;
    aarch64|arm64) arch="aarch64" ;;
  esac
  if [ -n "$os" ] && [ -n "$arch" ]; then
    echo "${arch}-${os}"
  else
    echo ""
  fi
}

TARGET=$(detect_target)

# ── resolve latest tag via GitHub API ─────────────────────────────────────
resolve_tag() {
  if [ "$TAG" != "latest" ]; then
    echo "$TAG"
    return
  fi
  local url="https://api.github.com/repos/${REPO_OWNER}/${REPO_NAME}/releases/latest"
  local tag
  tag=$(curl -fsSL "$url" 2>/dev/null | grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')
  if [ -z "$tag" ]; then
    warn "Could not determine latest release tag from GitHub API."
    warn "Use --tag <tag> to specify a version manually."
    return 1
  fi
  echo "$tag"
}

# ── install one binary from a release archive ─────────────────────────────
install_binary() {
  local bin="$1"     # e.g. grain-tui
  local tag="$2"
  local asset="${bin}-${tag}-${TARGET}.tar.gz"
  local url="https://github.com/${REPO_OWNER}/${REPO_NAME}/releases/download/${tag}/${asset}"

  say "Downloading ${asset} …"
  local tmpdir
  tmpdir=$(mktemp -d)
  # shellcheck disable=SC2064
  trap "rm -rf '$tmpdir'" EXIT

  local tarball="${tmpdir}/${asset}"
  if ! curl -fsSL --retry 3 --retry-delay 2 -o "$tarball" "$url"; then
    warn "Binary not found at ${url}"
    return 1
  fi

  info "Extracting …"
  tar -xzf "$tarball" -C "$tmpdir"

  # Find the binary in the extracted files
  local extracted
  extracted=$(find "$tmpdir" -type f -name "$bin" -perm +111 2>/dev/null | head -1)
  if [ -z "$extracted" ]; then
    # Maybe the binary is inside without exec perm yet
    extracted=$(find "$tmpdir" -type f -name "$bin" 2>/dev/null | head -1)
  fi
  if [ -z "$extracted" ]; then
    warn "Could not find ${bin} in the release archive."
    warn "Archive contents:"
    tar -tzf "$tarball" | head -20
    return 1
  fi

  chmod +x "$extracted" 2>/dev/null || true
  mv "$extracted" "${INSTALL_DIR}/${bin}"
  rm -rf "$tmpdir"
  return 0
}

# ── install one binary from source (cargo) ────────────────────────────────
install_source() {
  local crate="$1"
  local bin="$2"

  local feat_arg=""
  if [ "${NO_DEFAULT_FEATURES:-}" = "1" ]; then
    feat_arg="--no-default-features"
  fi
  if [ -n "${FEATURES:-}" ]; then
    feat_arg="$feat_arg --features ${FEATURES}"
  fi

  say "Building ${bin} from source (crate: ${crate}) …"

  # shellcheck disable=SC2086
  cargo install \
    --git "${REPO}" \
    --branch "${REF}" \
    $feat_arg \
    --locked \
    "${crate}" 2>&1 | while IFS= read -r line; do
      case "${line}" in
        *Installed*|*warning*|*error*:*|*Error:*|*Finished*) printf "   %s\n" "${line}" ;;
        *) : ;;
      esac
    done
}

# ── main ──────────────────────────────────────────────────────────────────
main() {
  header

  # ---- check required tools -------------------------------------------
  if [ "$INSTALL_MODE" != "binary" ]; then
    if ! command -v cargo >/dev/null 2>&1; then
      if [ "$INSTALL_MODE" = "source" ]; then
        warn "cargo not found — cannot build from source."
        exit 1
      fi
      # auto mode: cargo missing, try binary only
      warn "cargo not found, will try prebuilt binaries only."
      INSTALL_MODE="binary"
    else
      say "cargo $(cargo --version | cut -d' ' -f2)"
    fi
  fi

  if ! command -v curl >/dev/null 2>&1; then
    warn "curl is required but not found."
    exit 1
  fi

  # ---- target dir -----------------------------------------------------
  if [ ! -d "$INSTALL_DIR" ]; then
    mkdir -p "$INSTALL_DIR"
    say "created $INSTALL_DIR"
  fi

  # ---- resolve tag (for binary installs) ------------------------------
  local resolved_tag=""
  if [ "$INSTALL_MODE" != "source" ]; then
    resolved_tag=$(resolve_tag) || {
      if [ "$INSTALL_MODE" = "binary" ]; then
        exit 1
      fi
      warn "Falling back to source build."
      INSTALL_MODE="source"
    }
  fi

  # ---- summary --------------------------------------------------------
  info "Repository:   ${REPO}"
  if [ "$INSTALL_MODE" = "source" ]; then
    info "Mode:          source (cargo install --git, ref=${REF})"
  else
    info "Mode:          ${INSTALL_MODE}"
    info "Release tag:   ${resolved_tag}"
    info "Platform:      ${TARGET:-unknown}"
  fi
  info "Components:    ${COMPONENTS}"
  info "Install dir:   ${INSTALL_DIR}"

  local bins=""
  for comp in ${COMPONENTS}; do
    local b
    b=$(bin_name "$comp")
    if [ -n "$b" ]; then
      bins="$bins $b"
    fi
  done
  # trim leading space
  bins="${bins# }"
  info "Will install:  ${bins}"
  echo ""

  # ---- confirm --------------------------------------------------------
  if [ "${GRAIN_SKIP_CONFIRM}" != "1" ] && [ "${GRAIN_DRY_RUN}" != "1" ]; then
    printf "${CYAN}Proceed with install? [Y/n] ${NC}"
    read -r answer
    case "${answer:-y}" in
      [Yy]*) ;;
      *) info "aborted."; exit 0 ;;
    esac
  fi

  if [ "${GRAIN_DRY_RUN}" = "1" ]; then
    info "dry-run — would run:"
    for comp in ${COMPONENTS}; do
      local crate
      local bin
      crate=$(crate_name "$comp")
      bin=$(bin_name "$comp")
      if [ -z "$crate" ]; then
        warn "unknown component: ${comp}"
        continue
      fi
      if [ "$INSTALL_MODE" = "source" ]; then
        info "  cargo install --git ${REPO} --branch ${REF} ${crate}"
      else
        info "  download https://github.com/${REPO_OWNER}/${REPO_NAME}/releases/download/${resolved_tag}/${bin}-${resolved_tag}-${TARGET}.tar.gz"
        info "  extract → ${INSTALL_DIR}/${bin}"
      fi
    done
    exit 0
  fi

  # ---- install --------------------------------------------------------
  local failed=0
  for comp in ${COMPONENTS}; do
    local crate
    local bin
    crate=$(crate_name "$comp")
    bin=$(bin_name "$comp")

    if [ -z "$crate" ]; then
      warn "unknown component: ${comp} — skipping (expected: tui, headless)"
      failed=1
      continue
    fi

    echo ""
    echo -e "${BOLD}── ${bin} ──${NC}"

    local installed=false

    # Try binary first (auto / binary mode)
    if [ "$INSTALL_MODE" != "source" ] && [ -n "$resolved_tag" ] && [ -n "$TARGET" ]; then
      if install_binary "$bin" "$resolved_tag"; then
        installed=true
      elif [ "$INSTALL_MODE" = "binary" ]; then
        warn "${bin} binary download failed and --from-binary set — aborting."
        failed=1
        continue
      else
        warn "Binary download failed, falling back to source build …"
      fi
    elif [ "$INSTALL_MODE" != "source" ] && [ -z "$TARGET" ]; then
      warn "Unsupported platform — no prebuilt binary for $(uname -s)/$(uname -m)."
      if [ "$INSTALL_MODE" = "binary" ]; then
        failed=1
        continue
      fi
      warn "Falling back to source build …"
    fi

    # Fall back to source
    if ! $installed; then
      if ! command -v cargo >/dev/null 2>&1; then
        warn "cargo not found — cannot build from source."
        failed=1
        continue
      fi
      install_source "$crate" "$bin"
    fi

    # Verify
    if [ -x "${INSTALL_DIR}/${bin}" ]; then
      say "${bin} ${GREEN}✓${NC} installed"
    else
      warn "${bin} install may have failed — binary not found at ${INSTALL_DIR}/${bin}"
      failed=1
    fi
  done

  # ---- post-install ---------------------------------------------------
  echo ""
  if [ "${failed}" -eq 0 ]; then
    printf "${BOLD}${GREEN}Done!${NC}\n"
  else
    printf "${BOLD}${RED}Done with errors.${NC}\n"
  fi
  echo ""
  if ! echo "${PATH}" | grep -qF "${INSTALL_DIR}"; then
    warn "${INSTALL_DIR} is not on your PATH."
    printf " Add this to your shell rc file (~/.bashrc, ~/.zshrc):\n"
    printf " ${CYAN}export PATH=\"\${CARGO_HOME:-\$HOME/.cargo}/bin:\$PATH\"${NC}\n"
    echo ""
  fi
  info "Get started:  grain-tui --help"
  info "              grain-headless --help"
  info "Docs:         https://github.com/grainbook/grain-agent#readme"
  echo ""
  exit "${failed}"
}

main "$@"
