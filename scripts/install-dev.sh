#!/bin/bash
#######################################################
# ionterm dev install script developed by Lucasion #
#######################################################
# Installs the development build as a desktop app for the current user, so
# launchers and taskbars (Ionix, Hyprland) find it and show its icon:
#   ~/.local/share/applications/ionterm.desktop   (runs target/release/ionterm)
#   ~/.local/share/icons/hicolor/*/apps/ionterm.*
#
# Usage: scripts/install-dev.sh [install|uninstall]

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
APPS_DIR="$DATA_HOME/applications"
ICONS_DIR="$DATA_HOME/icons/hicolor"
BINARY="$ROOT/target/release/ionterm"
SIZES=(16 24 32 48 64 128 256 512)

# Prints a message in green
printf_green() {
  printf '\033[32m%s\033[0m\n' "$1"
}

# Prints a message in red
printf_red() {
  printf '\033[31m%s\033[0m\n' "$1"
}

# Refreshes the icon and desktop entry caches, if the tools exist
refresh_caches() {
  command -v gtk-update-icon-cache &>/dev/null && gtk-update-icon-cache -q -f -t "$ICONS_DIR" 2>/dev/null
  command -v update-desktop-database &>/dev/null && update-desktop-database -q "$APPS_DIR" 2>/dev/null
  return 0
}

# Copies the icons and writes a desktop entry pointing at the dev build
install_dev() {
  if [[ ! -x "$BINARY" ]]; then
    printf_red "No release build at $BINARY. Build it first with: cargo build --release"
    return 1
  fi
  echo "Installing icons..."
  install -Dm644 "$ROOT/data/icons/ionterm.svg" "$ICONS_DIR/scalable/apps/ionterm.svg"
  local SIZE
  for SIZE in "${SIZES[@]}"; do
    install -Dm644 "$ROOT/data/icons/hicolor/${SIZE}x${SIZE}/apps/ionterm.png" "$ICONS_DIR/${SIZE}x${SIZE}/apps/ionterm.png"
  done
  echo "Installing desktop entry..."
  mkdir -p "$APPS_DIR"
  sed "s|^Exec=ionterm|Exec=$BINARY|" "$ROOT/data/ionterm.desktop" > "$APPS_DIR/ionterm.desktop"
  refresh_caches
  printf_green "Done! Installed ionterm (dev build) to $APPS_DIR/ionterm.desktop"
}

# Removes what install_dev added
uninstall_dev() {
  echo "Removing desktop entry and icons..."
  rm -f "$APPS_DIR/ionterm.desktop" "$ICONS_DIR/scalable/apps/ionterm.svg"
  local SIZE
  for SIZE in "${SIZES[@]}"; do
    rm -f "$ICONS_DIR/${SIZE}x${SIZE}/apps/ionterm.png"
  done
  refresh_caches
  printf_green "Done! Removed the ionterm dev install"
}

case "${1:-install}" in
  install) install_dev ;;
  uninstall) uninstall_dev ;;
  help | -h | --help)
    echo "Installs the ionterm dev build as a desktop app for the current user."
    echo "Usage: scripts/install-dev.sh [install|uninstall]"
    ;;
  *)
    printf_red "Command not found: $1"
    echo "Usage: scripts/install-dev.sh [install|uninstall]"
    exit 2
    ;;
esac
