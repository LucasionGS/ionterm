#!/bin/bash
# Guarded driver for test ionterm windows. Every key and kill is checked
# against the pid recorded by `launch`, which must be a live window of
# class `ionterm` whose process is an ionterm binary, and never an ancestor
# of this shell (the user's kitty running Claude Code).
#
#   drive.sh launch "<exec cmd>"   start a window, float + resize it
#   drive.sh key "<MODS>" <key>     send one key to that window only
#   drive.sh shot <file.png>        screenshot that window
#   drive.sh close                  kill that window's process only
set -u
STATE="${XDG_RUNTIME_DIR:-/tmp}/ionterm-drive.pid"

ionterm_pids() {
  hyprctl clients -j | python3 -c "import json,sys; print(' '.join(str(c['pid']) for c in json.load(sys.stdin) if c['class']=='ionterm'))"
}

# Aborts unless $1 is safe to target.
verify() {
  local PID="$1"
  [[ "$PID" =~ ^[0-9]+$ ]] || { echo "ABORT: no valid pid recorded ('$PID')" >&2; exit 9; }
  local p=$$
  while [[ "$p" -gt 1 ]]; do
    [[ "$p" == "$PID" ]] && { echo "ABORT: pid $PID is an ancestor of this shell" >&2; exit 9; }
    p=$(ps -o ppid= -p "$p" | tr -d ' ')
  done
  [[ "$(ps -o comm= -p "$PID")" == "ionterm" ]] || { echo "ABORT: pid $PID is not an ionterm process" >&2; exit 9; }
  [[ " $(ionterm_pids) " == *" $PID "* ]] || { echo "ABORT: pid $PID has no ionterm window" >&2; exit 9; }
}

case "$1" in
  launch)
    before=" $(ionterm_pids) "
    rm -f "$STATE"
    hyprctl dispatch "hl.dsp.exec_cmd(\"$2\")" >/dev/null
    PID=""
    for _ in $(seq 30); do
      sleep 0.2
      for p in $(ionterm_pids); do
        [[ "$before" == *" $p "* ]] || PID=$p
      done
      [[ -n "$PID" ]] && break
    done
    verify "$PID"
    echo "$PID" > "$STATE"
    hyprctl dispatch "hl.dsp.window.float({ action = \"enable\", window = \"pid:$PID\" })" >/dev/null
    hyprctl dispatch "hl.dsp.window.resize({ x = ${3:-1200}, y = ${4:-650}, window = \"pid:$PID\" })" >/dev/null
    sleep 1
    echo "launched $PID"
    ;;
  key)
    PID=$(cat "$STATE" 2>/dev/null); verify "$PID"
    hyprctl dispatch "hl.dsp.send_shortcut({ mods = \"$2\", key = \"$3\", window = \"pid:$PID\" })" >/dev/null
    sleep 0.3
    ;;
  shot)
    PID=$(cat "$STATE" 2>/dev/null); verify "$PID"
    G=$(hyprctl clients -j | python3 -c "import json,sys; c=[c for c in json.load(sys.stdin) if c['pid']==$PID][0]; print('%d,%d %dx%d' % (*c['at'], *c['size']))")
    grim -g "$G" "$2"
    ;;
  close)
    PID=$(cat "$STATE" 2>/dev/null); verify "$PID"
    kill "$PID"; rm -f "$STATE"
    ;;
  *) echo "Command not found: $1" >&2; exit 1 ;;
esac
