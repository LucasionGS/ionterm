#!/bin/bash
#######################################################
# kitty → ionterm theme converter developed by Lucasion #
#######################################################
# Converts the colour keys of a kitty.conf into an ionterm theme.toml. Values
# are copied verbatim, so adaptive templates keep their {{slot}} tokens.
#
# Usage: kitty-to-ionterm.sh <kitty.conf> [theme.toml]   (stdout when omitted)

# Prints the value of a kitty.conf key, or nothing when it is missing
kitty_value() {
  local FILE="$1"
  local KEY="$2"
  awk -v key="$KEY" '$1 == key { print $2; exit }' "$FILE"
}

# Prints `name = "value"` when the kitty key exists
toml_color() {
  local NAME="$1"
  local VALUE="$2"
  [[ -n "$VALUE" ]] && printf '%s = "%s"\n' "$NAME" "$VALUE"
}

# Writes the full theme.toml for a kitty.conf to stdout
convert_theme() {
  local FILE="$1"
  local OPACITY
  OPACITY="$(kitty_value "$FILE" background_opacity)"

  echo "# ionterm theme, generated from $(basename "$(dirname "$FILE")")/$(basename "$FILE")"
  toml_color foreground "$(kitty_value "$FILE" foreground)"
  toml_color background "$(kitty_value "$FILE" background)"
  [[ -n "$OPACITY" ]] && printf 'background_opacity = %s\n' "$OPACITY"
  toml_color cursor "$(kitty_value "$FILE" cursor)"
  toml_color cursor_text "$(kitty_value "$FILE" cursor_text_color)"
  toml_color selection_foreground "$(kitty_value "$FILE" selection_foreground)"
  toml_color selection_background "$(kitty_value "$FILE" selection_background)"
  toml_color url "$(kitty_value "$FILE" url_color)"
  toml_color active_border "$(kitty_value "$FILE" active_border_color)"
  toml_color inactive_border "$(kitty_value "$FILE" inactive_border_color)"

  echo
  echo "[tab_bar]"
  toml_color background "$(kitty_value "$FILE" tab_bar_background)"
  toml_color active_foreground "$(kitty_value "$FILE" active_tab_foreground)"
  toml_color active_background "$(kitty_value "$FILE" active_tab_background)"
  toml_color inactive_foreground "$(kitty_value "$FILE" inactive_tab_foreground)"
  toml_color inactive_background "$(kitty_value "$FILE" inactive_tab_background)"

  echo
  echo "[palette]"
  echo "colors = ["
  local I
  for I in $(seq 0 15); do
    printf '  "%s",\n' "$(kitty_value "$FILE" "color$I")"
  done
  echo "]"
}

if [[ -z "$1" || "$1" == "-h" || "$1" == "--help" ]]; then
  echo "Converts a kitty.conf's colours into an ionterm theme.toml."
  echo "Usage: kitty-to-ionterm.sh <kitty.conf> [theme.toml]"
  [[ -z "$1" ]] && exit 2
  exit 0
fi
if [[ ! -f "$1" ]]; then
  echo "File not found: $1" >&2
  exit 1
fi

if [[ -n "$2" ]]; then
  mkdir -p "$(dirname "$2")"
  convert_theme "$1" > "$2"
else
  convert_theme "$1"
fi
