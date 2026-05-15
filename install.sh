#!/usr/bin/env bash
# pgea installer.
#
#   curl -fsSL https://raw.githubusercontent.com/ra100/pgea/main/install.sh | bash
#
# Env vars:
#   PGEA_VERSION   tag to install (e.g. v0.2.0). Default: latest release.
#   PGEA_BIN_DIR   install dir for the binary. Default: $HOME/.local/bin.
#   PGEA_REPO      override repo (owner/name). Default: ra100/pgea.
#   PGEA_NO_VERIFY non-empty to skip sha256 check. Default: verify.
#   PGEA_FORCE     non-empty to overwrite existing binary without prompt.

set -euo pipefail

REPO="${PGEA_REPO:-ra100/pgea}"
BIN_DIR="${PGEA_BIN_DIR:-$HOME/.local/bin}"
VERSION="${PGEA_VERSION:-}"
NO_VERIFY="${PGEA_NO_VERIFY:-}"
FORCE="${PGEA_FORCE:-}"

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m %s\n'  "$*" >&2; }
die()  { printf '\033[1;31mxx\033[0m %s\n'  "$*" >&2; exit 1; }

require() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

require curl
require tar
require uname
require mkdir
require install

detect_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Darwin)
      case "$arch" in
        arm64|aarch64) echo "aarch64-apple-darwin" ;;
        x86_64) die "macOS x86_64 is not built; use Rosetta with the arm64 binary, or build from source" ;;
        *) die "unsupported macOS arch: $arch" ;;
      esac
      ;;
    Linux)
      case "$arch" in
        x86_64|amd64) echo "x86_64-unknown-linux-gnu" ;;
        aarch64|arm64) die "linux aarch64 not yet built; open issue at https://github.com/${REPO}/issues" ;;
        *) die "unsupported Linux arch: $arch" ;;
      esac
      ;;
    *) die "unsupported OS: $os (Windows: use WSL or build from source)" ;;
  esac
}

resolve_latest() {
  # Resolve the GitHub "releases/latest" redirect to a tag without hitting the API.
  # When no Release is published, GitHub redirects to /releases (no /tag/ segment);
  # detect that and fall back to the API.
  local url tag
  url="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest")"
  case "$url" in
    */releases/tag/*) tag="${url##*/}" ;;
    *) tag="" ;;
  esac
  if [ -z "$tag" ]; then
    # Tolerate failure: API returns 404 when no Release exists, and `head` closing
    # the pipe early would also trip `set -o pipefail`.
    tag="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null \
      | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1 || true)"
  fi
  if [ -z "$tag" ]; then
    die "no published release found for ${REPO}. Set PGEA_VERSION=vX.Y.Z (see https://github.com/${REPO}/releases) or wait for one to be published."
  fi
  echo "$tag"
}

verify_sha() {
  local archive="$1" expected
  expected="$(awk '{print $1}' "${archive}.sha256")"
  if command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$archive" | awk '{print $1}')"
  elif command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$archive" | awk '{print $1}')"
  else
    warn "no sha256 tool found; skipping verify"
    return 0
  fi
  [ "$expected" = "$actual" ] || die "sha256 mismatch: expected $expected, got $actual"
}

tmp=""
cleanup() { [ -n "${tmp:-}" ] && rm -rf "$tmp"; }
trap cleanup EXIT

main() {
  local target tag asset_url sha_url archive bin_path

  target="$(detect_target)"
  tag="${VERSION:-$(resolve_latest)}"
  [ -n "$tag" ] || die "could not resolve latest tag; set PGEA_VERSION=vX.Y.Z"
  case "$tag" in v*) ;; *) die "tag must start with 'v' (got: $tag)" ;; esac

  log "repo:    $REPO"
  log "version: $tag"
  log "target:  $target"
  log "bin dir: $BIN_DIR"

  asset_url="https://github.com/${REPO}/releases/download/${tag}/pgea-${tag}-${target}.tar.gz"
  sha_url="${asset_url}.sha256"

  tmp="$(mktemp -d)"

  archive="${tmp}/pgea.tar.gz"
  log "fetching $asset_url"
  curl -fsSL "$asset_url" -o "$archive" || die "download failed: $asset_url"

  if [ -z "$NO_VERIFY" ]; then
    log "fetching checksum"
    curl -fsSL "$sha_url" -o "${archive}.sha256" || die "checksum download failed: $sha_url"
    verify_sha "$archive"
    log "sha256 ok"
  fi

  tar -C "$tmp" -xzf "$archive"
  [ -f "${tmp}/pgea" ] || die "archive missing pgea binary"

  mkdir -p "$BIN_DIR"
  bin_path="${BIN_DIR}/pgea"
  if [ -e "$bin_path" ] && [ -z "$FORCE" ] && [ -t 0 ]; then
    printf 'overwrite %s? [y/N] ' "$bin_path"
    read -r ans
    case "$ans" in y|Y|yes|YES) ;; *) die "aborted" ;; esac
  fi
  install -m 0755 "${tmp}/pgea" "$bin_path"

  log "installed: $bin_path"

  case ":$PATH:" in
    *":${BIN_DIR}:"*) ;;
    *) warn "$BIN_DIR not in PATH. Add to your shell profile:"
       printf '  export PATH="%s:$PATH"\n' "$BIN_DIR" ;;
  esac

  log "verify: pgea --version"
  log "update: pgea self-update   (or rerun this installer)"
}

main "$@"
