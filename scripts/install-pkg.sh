#!/bin/bash
#########################################################
# ionterm package install script developed by Lucasion #
#########################################################
# Builds and installs the ionterm-git package from the PKGBUILD in a
# temporary folder under ~/.cache, so makepkg's src/, pkg/, source clone and
# package file never land in the working directory. The folder is removed
# afterwards, whether the build worked or not.
#
# The package builds what's on GitHub, not this folder: with unpushed
# commits or uncommitted changes it asks before building.
#
# Usage: scripts/install-pkg.sh [makepkg options]   (default: -si)

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/ionterm"
# The temporary build folder, global so the exit trap still knows it.
BUILD_DIR=""

# Prints a message in green
printf_green() {
  printf '\033[32m%s\033[0m\n' "$1"
}

# Prints a message in red
printf_red() {
  printf '\033[31m%s\033[0m\n' "$1"
}

# Asks a yes/no question; only "y" says yes
confirm() {
  local ANSWER
  read -r -p "$1 [y/N]: " ANSWER
  [[ "$ANSWER" == "y" ]]
}

# Warns about work the GitHub build won't have, and asks whether to go on
check_pushed() {
  local AHEAD DIRTY
  AHEAD="$(git -C "$ROOT" rev-list --count '@{u}..HEAD' 2>/dev/null || echo 0)"
  DIRTY="$(git -C "$ROOT" status --porcelain --untracked-files=no 2>/dev/null)"
  if [[ "$AHEAD" -eq 0 && -z "$DIRTY" ]]; then
    return 0
  fi
  if [[ "$AHEAD" -gt 0 ]]; then
    printf_red "$AHEAD commit(s) aren't pushed yet; the package builds from GitHub and won't have them."
  fi
  if [[ -n "$DIRTY" ]]; then
    printf_red "There are uncommitted changes; the package won't have them either."
  fi
  confirm "Build what's on GitHub anyway?"
}

# Builds the package in a temporary folder and runs makepkg with the given options
install_pkg() {
  local STATUS VERSION
  mkdir -p "$CACHE_DIR"
  BUILD_DIR="$(mktemp -d "$CACHE_DIR/pkg-build.XXXXXX")" || return 1
  # Removed however makepkg ends, ctrl+c included.
  trap '[[ -n "$BUILD_DIR" ]] && rm -rf -- "$BUILD_DIR"' EXIT
  cp "$ROOT/PKGBUILD" "$BUILD_DIR/"
  echo "Building ionterm-git in $BUILD_DIR..."
  (cd "$BUILD_DIR" && makepkg "$@")
  STATUS=$?
  if [[ $STATUS -ne 0 ]]; then
    printf_red "makepkg failed (exit $STATUS)."
    return $STATUS
  fi
  VERSION="$(sed -n 's/^pkgver=//p' "$BUILD_DIR/PKGBUILD")"
  printf_green "Done! Built ionterm-git ${VERSION:-} (makepkg $*); the build folder is cleaned up."
}

if [[ ! -f /etc/arch-release ]]; then
  printf_red "This builds an Arch package with makepkg. Elsewhere, build with: cargo build --release"
  exit 1
fi
if ! command -v makepkg &>/dev/null; then
  printf_red "makepkg not found. Install it with: sudo pacman -S base-devel"
  exit 1
fi

ARGS=("$@")
if [[ ${#ARGS[@]} -eq 0 ]]; then
  ARGS=(-si)
fi
check_pushed || exit 1
install_pkg "${ARGS[@]}"
exit $?
