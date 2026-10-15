#!/bin/bash
# Sends real keystrokes to a fresh ionterm window through Hyprland and prints
# the bytes the application received, in legacy mode and three kitty keyboard
# protocol modes. Keys go to that window by PID, so focus doesn't matter.
#
# Usage: scripts/keytest.sh [path/to/ionterm]   (default: target/release/ionterm)

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IONTERM="${1:-$ROOT/target/release/ionterm}"
LOG="$(mktemp)"
KEYS=("|a" "SHIFT|a" "CTRL|a" "ALT|x" "|Return" "SHIFT|Return" "|Escape" "|Up" "CTRL|Up" "|F1" "|F5" "SHIFT|Tab" "|BackSpace" "CTRL|BackSpace")

if ! command -v hyprctl &>/dev/null; then
  echo "This test needs Hyprland (hyprctl not found)." >&2
  exit 1
fi
if [[ ! -x "$IONTERM" ]]; then
  echo "ionterm not found at $IONTERM. Build it with: cargo build --release" >&2
  exit 1
fi

# Sends one key combination to the window owned by $PID
send_key() {
  local MODS="$1"
  local KEY="$2"
  hyprctl dispatch "hl.dsp.send_shortcut({ mods = \"$MODS\", key = \"$KEY\", window = \"pid:$PID\" })" > /dev/null
  sleep 0.15
}

"$IONTERM" -e python3 "$ROOT/scripts/keytest.py" "$LOG" > /dev/null 2>&1 &
PID=$!
sleep 1.5
for _ in 0 1 2 3; do
  for ENTRY in "${KEYS[@]}"; do
    send_key "${ENTRY%%|*}" "${ENTRY#*|}"
  done
  sleep 1.6
done
wait "$PID"

I=0
while read -r LINE; do
  if [[ "$LINE" == PHASE* ]]; then
    echo "--- kitty flags ${LINE#PHASE }" | cut -d' ' -f1-4
    I=0
  else
    ENTRY="${KEYS[$I]:-?|?}"
    printf '  %-16s %s\n' "${ENTRY%%|*}+${ENTRY#*|}" "${LINE#* }"
    I=$((I + 1))
  fi
done < "$LOG"
rm -f "$LOG"
