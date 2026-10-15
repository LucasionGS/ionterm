#!/bin/bash
# ionterm section for the Ionix styler. Paste these functions into
# styler/install-default, next to ionix_install_kitty.

# Links the active theme's ionterm colours into ~/.config/ionterm/theme.toml
ionix_install_ionterm() {
  local dest="$HOME/.config/ionterm"
  local theme="$dest/theme.toml"
  mkdir -p "$dest"

  # A real file here is the user's own palette; keep it, numbered, rather than
  # overwrite it (same reasoning as ioexplorer's theme.css).
  if [[ -f "$theme" && ! -L "$theme" ]]; then
    local backup="$theme.bak"
    local n=1
    while [[ -e "$backup" ]]; do
      backup="$theme.bak.$((n++))"
    done
    msg "ionterm: existing theme.toml found, backing up to $(basename "$backup")"
    mv "$theme" "$backup"
  fi

  ln -sf "$(ionix_theme_file ionterm theme.toml)" "$theme"
  ionix_reload_ionterm
}

# Removes our symlink, leaving any file the user made themselves
ionix_uninstall_ionterm() {
  local theme="$HOME/.config/ionterm/theme.toml"
  if [[ -L "$theme" ]]; then
    rm -f "$theme"
  elif [[ -e "$theme" ]]; then
    msg "ionterm: theme.toml is not our symlink, keeping it"
  fi
}

# ionterm watches ~/.config/ionterm and reloads on its own, so re-linking the
# file is the reload. Kept for symmetry with the other sections.
ionix_reload_ionterm() {
  return 0
}
