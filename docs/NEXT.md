# ionterm: handoff for the next session

Start a fresh session in `/home/ion/development/ionterm` with something like:

> Read `docs/NEXT.md`, then pick up the next task.

This file has what a new session needs: what exists, how the user likes things, how to test without bothering them, and the next tasks in detail. `docs/PLAN.md` is the full roadmap with checkboxes. Keep both up to date as tasks finish.

## What ionterm is

A GPU-rendered terminal emulator (Rust) for the user's Ionix desktop (Arch Linux + Hyprland). It's kitty-like, with Windows Terminal-style tabs and Tabby-style SSH. The app, binary, crates and folder are all `ionterm` (the name `termion` is taken by a popular crate).

Done so far, in commit order (`git log --oneline`):

1. **v0.1 core:**
   - wgpu renderer, VT parser with reflow, scrollback, selection and clipboard.
   - kitty keyboard protocol, mouse reporting, OSC 8 links, OSC 133 prompt marks.
   - Prompt-aware resize: the shell's prompt is blanked rather than reflowed, so fish/oh-my-posh don't leave stray copies.
   - TOML config and theme, both live-reloaded.
2. **Rendering:** text matches kitty (linear-light blending through an offscreen `Rgba16Float` frame plus an sRGB blit pass, and unhinted glyphs). The default background is `#0b0b0f`.
3. **Tabs and splits:**
   - Windows Terminal-style tab bar: top, shown with 2+ tabs, × and + buttons, smooth drag-to-reorder animation, double-click to rename.
   - Right-click menu: rename, duplicate, colour swatches, close, close others.
   - Process icons in tabs. kitty Powerline style is still an option.
   - Layouts: splits, tall, fat, grid, stack.
4. **Images:** the kitty graphics protocol (including unicode placeholders) and Sixel. Images are anchored to rows, so they scroll and reflow with the text.
5. **SSH phases 1 and 2:**
   - The `ctrl+shift+s` picker over `~/.ssh/config`, the `⌄` button beside `+`, and `ionterm ssh <host>`.
   - A remote bootstrap that sets up terminfo and an OSC 7 directory hook.
   - A reconnect banner, per-host colour/icon/group, and recognition of `ssh` typed in a shell.
   - A connection editor that saves to `~/.ssh/config.d/ionterm`.
6. **SSH phase 3, task 1 (file panel):** `files/` (`remote.rs` runs commands over the shared connection and parses listings, `transfer.rs` runs downloads and uploads on threads, `panel.rs` is the widget), `app/files.rs` (the app glue: requests, conflicts, toasts, the floating transfer card) and `dialog.rs` (a generic modal with buttons, an optional input and a checkbox).
7. **SSH phase 3, task 2 (drag and drop):** `dnd/` (`wayland.rs` binds our own `wl_data_device` on winit's display, on its own queue and thread; `App::exiting` must drop the listener before winit disconnects) and `app/drag.rs` (drop targets, the highlight while dragging, upload or paste).
8. **SSH phase 3, task 3 (rsync, ZMODEM, `ionterm-get`/`ionterm-put`):** `files/rsync.rs`, `zmodem/` (`frame.rs` wire format, `mod.rs` detector and session) wired into `pty.rs`'s reader, `app/rzsz.rs`, and OSC 5379 in `ionterm-vt`. See "SSH phase 3: files" below.
9. **Shell integration (v0.3b):** OSC 133 marks on rows (`Row::prompt`, `output`, `status`; carried through reflow) and the queries on `Term` (`prompt_lines`, `command_output_at`, `last_command_output`, `failed_commands_in_view`, `click_goes_to_shell`, `progress`, `progress_hint`). `shell.rs` injects `extras/shell-integration/ionterm.{bash,zsh,fish}` into the local login shell (bash `--rcfile`, zsh `ZDOTDIR`, fish 3 `-C`; fish 4 needs nothing), and `ssh/bootstrap.rs` embeds the same files. `app/integration.rs` has the prompt jumps, output copy/select, failed marks and the tab progress bars (`tabbar::draw_progress`).
10. **Hints mode (v0.3b):** `hints.rs` (finding URLs, paths, `file:line`, hashes and IPs in a line; labels; editor commands, testable without a window) and `app/hints.rs` (the overlay, keys, and open/copy/insert). Local paths are checked against the shell's directory; SSH panes can't be checked, so only clear paths count there and picking one copies it.
11. **Settings tab:** `settings.rs` (the list of settings and their controls, page state, keys, layout and drawing; testable without a window), `app/settings.rs` (opening the tab, writing with `config::write_setting`, the font picker's families from the renderer, and `$EDITOR` in the tab). A settings tab is a normal `Tab` with `settings: Some(page)`; its one pane runs an idle `sh` loop while the page shows, and `Pane::run` swaps in the editor and back (`settings_child_exited` ignores the old program's late "exited" with `Pane::is_running`). Two renderer fixes came with it: glyphs snap to whole pixels (UI text at half a cell was garbled), and the atlas is only cleared when the font really changed.

## Code map

```
crates/ionterm-vt/       terminal state, no I/O except kitty graphics files/shm
  term.rs                Term (vte::Perform), Processor (APC pre-parser), modes, OSC/CSI/DCS, graphics commands
  reflow.rs              re-wrapping lines (carries prompt marks + image anchors)
  graphics.rs            kitty graphics store/placements; sixel.rs decoder; diacritics.rs placeholders
  cell.rs                Cell, Row (wrapped, prompt, anchors)
crates/ionterm-render/   wgpu renderer
  lib.rs                 Renderer::render(&[PaneView], &Chrome); instanced quads; KIND_SOLID/MASK/COLOR/ROUNDED/IMAGE
  shader.wgsl / blit.wgsl  linear-light frame, then sRGB blit
  font.rs (cosmic-text run shaping), boxdraw.rs (procedural box/block/Powerline), images.rs (GPU textures)
  ChromeItem::{Rect, RoundedRect, Text}: generic UI primitives the app uses for all its own UI
crates/ionterm/          the app
  app.rs                 winit ApplicationHandler, TermWindow (tabs, palette, editor, menu, drag), all event routing
  pane.rs                one shell: Term + Pty, selection, hover, SSH connection state, reconnect
  tab.rs / layout.rs     tabs, split tree, layouts
  tabbar.rs / menu.rs / palette.rs / editor.rs   UI widgets: layout(), hit(), draw() built from ChromeItems
  ssh/                   config.rs (parse ~/.ssh/config + Include), bootstrap.rs (remote script),
                         detect.rs (typed ssh argv), store.rs (save/delete hosts, Include line, hosts.toml), mod.rs (Connection argv, ControlMaster)
  pty.rs                 portable-pty spawn (OutputSink), reader thread (hands ZMODEM to a zmodem::Session), exit_code, foreground process
  files/                 remote.rs (commands over the shared connection, listings), transfer.rs (threads, progress, cancel),
                         rsync.rs (used when both ends have rsync), panel.rs (the side panel widget)
  zmodem/                frame.rs (headers, subpackets, CRCs, decoder), mod.rs (Detector, Session: sz → us, us → rz)
  dnd/                   drops: mod.rs (uri-list, shell quoting), wayland.rs (own wl_data_device; winit 0.30 has none)
  dialog.rs              generic modal: buttons, optional input and checkbox
  app/                   files.rs (panel requests, conflicts, toasts, transfer card), drag.rs (drop targets), rzsz.rs (ZMODEM glue),
                         integration.rs (prompt jumps, output copy/select, failed marks, tab progress), hints.rs (hints overlay),
                         settings.rs (settings tab, config writes, $EDITOR in the tab)
  input.rs               key encoding (legacy + kitty), mouse encoding, KeyCombo, Action enum, DEFAULT_BINDINGS
  shell.rs               shell integration for the local login shell (scripts in extras/shell-integration/)
  remote.rs              `ionterm @`: socket, client, request parsing (commands run in app/remote.rs)
  session.rs             session restore: the TOML file, ownership between windows, the split tree as text (app/session.rs builds and reopens it)
  scrollback.rs          the scrollback as ANSI text for the pager (ctrl+shift+h), the pager command
  hints.rs               hints mode: finding URLs/paths/file:line/hashes/IPs, labels, editor commands
  settings.rs            the settings page: settings list, controls, keys, layout and drawing
  config.rs / theme.rs / links.rs
extras/                  terminfo source, Ionix styler theme files + install functions, example ionterm.toml
scripts/keytest.sh       real-keystroke test of the keyboard protocol (Hyprland)
scripts/drive.sh         guarded Hyprland driver for test windows (see Testing)
```

Patterns to follow:
- **UI widgets** each have `layout()` (pure geometry), `hit()` and `draw()`, plus key handling. App state lives on `TermWindow` as `Option<Widget>`. Modal widgets take input first in `window_event` and `tab_bar_click`.
- **Borrow-checker trick in `app.rs`:** to call `&mut self` methods after inspecting the window, return a small enum (`BarAction`, `FollowUp`, `Outcome`) from the borrowing code and act on it afterwards.

## How the user works (follow these)

**Conventions:**
- **Rust style:** standard rustfmt (4 spaces, `} else {`), matching their other Rust project, `/home/ion/development/ioexplorer`. Their global CLAUDE.md rules for TypeScript (2-space, `else` on a new line) don't apply here. Use `//!` module docs, `///` on items, and comments that explain *why*.
- **Before reporting work done, run all of these:**
  - `cargo build --release`
  - `cargo test --workspace`
  - `cargo clippy --workspace` (0 warnings)
  - `cargo check --target x86_64-pc-windows-msvc` (0 warnings; the target is installed; Windows must keep compiling)
- **Commits:**
  - Only commit when asked. They usually say "commit it", often bundled with the next request.
  - Message style: `Added X` / `Fixed Y`, a single line, **no Co-Authored-By or any AI attribution**.
  - Use plain `git commit -m`.
- **Docs:** update `docs/PLAN.md` checkboxes and the README feature list when something lands.
- **Questions:** they like being asked design questions with options (AskUserQuestion) before big features, and they answer them. Small fixes: just do them.

**Settled decisions (don't reverse them):**
- **New tabs and splits are always local shells.** Only "Duplicate tab" reconnects an SSH tab to the same host. (They asked for this explicitly.)
- **Never install the `xterm-ionterm` terminfo locally** (`~/.terminfo` or `/usr/share/terminfo`). With it installed, `TERM=xterm-ionterm` leaks into plain `ssh` and causes "unknown terminal", which they hate. `term = "auto"` therefore resolves to `xterm-256color` locally. ionterm-launched SSH connections install it on the server instead.
- **`advertise_sixel = false` by default:** announcing Sixel in DA1 makes yazi choose Sixel over the (better) kitty protocol.
- **Tabs:** Windows Terminal style, top, auto (2+ tabs). Background `#0b0b0f`. Text must keep looking like their kitty.

## Testing without bothering the user

**Their machine:**
- **System and shell:** Arch, Hyprland 0.56 (Lua config, so `hyprctl dispatch` takes Lua), fish 4.9 with an oh-my-posh prompt that has a right-hand block, kitty with JetBrainsMono Nerd Font 11pt, NVIDIA RTX 4070 on Vulkan.
- **SSH:** 51 hosts in `~/.ssh/config`, no Include before ionterm adds one. Key login to localhost does NOT work, and localhost's host key changed in `known_hosts`.
- **Tools installed:** `kitten`, `chafa`, `yazi`, ImageMagick `magick`, `sftp`, `scp`, `rsync` 3.5, `gamescope`, `xdotool`. Not installed: `lrzsz` (not on `ionserver` either).
- **Their test server:** `ionserver` (Ubuntu, bash 5.2, zsh and rsync 3.2.7 installed, no lrzsz) is fine to test against; they offered it. Clean up anything you create there.

**Driving ionterm through Hyprland: always use `scripts/drive.sh`.** Claude Code runs inside the user's **kitty**, which is normally the focused window. A dispatch with an empty or stale pid (`window = "pid:"`) can land on kitty, and test keys like ctrl+d, Escape or Return then hit Claude Code itself. A previous session lost its own terminal this way. `drive.sh` only sends keys to, or kills, the window it launched, after checking that it is a live `ionterm` window and not an ancestor of the shell. Never use `pkill`/`killall`, `killactive`, or a dispatch without a verified `window =`.
```bash
scripts/drive.sh launch "/home/ion/development/ionterm/target/release/ionterm"   # the user's real env; waits for the new window, floats it, 1200x650
scripts/drive.sh key "CTRL SHIFT" t     # to that window only; aborts (exit 9) if it is gone
scripts/drive.sh shot shot.png          # then Read the png to look at it
scripts/drive.sh close
```
- **Key names** are xkb names (`Return`, `BackSpace`, `period`, `Down`, …). `slash` did not type a `/`, so avoid needing one.
- **Testing the bootstrap's shell hooks** without a window: run the script under Python's `pty` and grep the output for `\e]7;` and `\e]5379;`. fish 4 waits at startup for answers to its terminal queries, so reply to `\e[0c` with `\e[?62;22c`, or fish never reads input.
- **When the screen is locked** (it locks after a while), `grim -g` captures the lock screen and the window stops redrawing (no frame callbacks). `grim -w <address>` captures one window but shows a stale frame. Use headless gamescope instead, which doesn't care about the lock: run ionterm inside it as an X11 client, type with `xdotool` on gamescope's display (`DISPLAY=:1`; `xdotool type` handles `/`), and screenshot with `gamescopectl screenshot <file>`:
  ```bash
  env -u WAYLAND_DISPLAY -u DISPLAY gamescope --backend headless -W 1200 -H 650 -w 1200 -h 650 -- env -u WAYLAND_DISPLAY <launcher.sh> &
  DISPLAY=:1 xdotool type -- 'sz huge.bin'; DISPLAY=:1 xdotool key Return
  gamescopectl screenshot shot.png
  ```
  Put the ionterm command in a launcher script rather than nesting quotes. **Mouse tests work there too**, since gamescope is its own X server and the user's pointer is never involved: `W=$(DISPLAY=:1 xdotool search --class ionterm)`, then `DISPLAY=:1 xdotool mousemove --window $W <x> <y> click 1` (window pixels; `keydown ctrl+shift click 3 keyup ctrl+shift` for modifiers). The 916×616 window is scaled into the 1200×650 screenshot (×1.055, offset 116 px). Drag and drop from a file manager still needs the user. Use a scratch `HOME`/`XDG_DATA_HOME` so commands don't land in the user's fish history.
- **Don't move the user's mouse.** Mouse-only features (drag, right-click menus, hover) need the user to verify; say so plainly.
- **Shell:** launching via `hl.dsp.exec_cmd` gives the user's real environment (fish). Launching from the Bash tool gives bash (`$SHELL` differs), so use `-e fish` there.

**Other tools:**
- **Scratch home:** `env HOME=$SCRATCH/fh XDG_CONFIG_HOME=$SCRATCH/fh/.config ionterm` tests anything that writes config (`~/.ssh`, hosts.toml) without touching real files. `dirs::home_dir()` follows `$HOME`.
- **Fake ssh:** put a script named `ssh` first in `PATH` that skips the options, then runs the remote command locally as host "fakehost", with a scratch `HOME` and a `fakebin/hostname` that prints `fakehost`:
  ```bash
  #!/bin/bash
  dest=""; cmd=""
  while [ $# -gt 0 ]; do case "$1" in -o|-p|-i|-l|-F|-J) shift 2;; -*) shift;; *) if [ -z "$dest" ]; then dest=$1; shift; else cmd="$*"; break; fi;; esac; done
  export HOME=$SCRATCH/fakehome PATH=$SCRATCH/fakebin:$PATH SHELL=/usr/bin/fish; cd "$HOME"
  if [ -n "$cmd" ]; then exec sh -c "$cmd"; else exec bash -l; fi
  ```
  Launch with `hl.dsp.exec_cmd("env PATH=$SCRATCH/bin:/usr/bin …/ionterm ssh fakehost")`. Don't connect to the user's real servers without asking (except `ionserver`): the bootstrap writes `~/.terminfo` there. rsync works through the fake ssh too (it runs `rsync --server` locally).
- **lrzsz for ZMODEM tests:** download Arch's `lrzsz` package from a mirror into its own scratch folder, verify it (`gpg --homedir /etc/pacman.d/gnupg --verify <pkg>.sig <pkg>`), and extract it there; don't install it. Link `lrzsz-sz`/`lrzsz-rz` as `sz`/`rz` in the fake host's `fakebin`. The session tests run against it with `IONTERM_LRZSZ=<dir>/usr/bin cargo test -p ionterm lrzsz -- --ignored`.
- **Transfers against a real host:** `IONTERM_TEST_HOST=ionserver cargo test -p ionterm round_trips_over_ssh -- --ignored` (rsync, the cat/tar fallback, and rsync cancels on both sides; it cleans up its `/tmp` folder).
- **Debugging and timing:**
  - `IONTERM_LOG=ionterm_vt=debug` logs unhandled escape sequences. Logs go to **stdout**, not stderr.
  - `IONTERM_DUMP=<file>` records raw PTY output.
  - For animations, capture `grim` frames ~50 ms apart and measure pixels with PIL.
- **Wayland redraws:** `app.rs` calls `window.pre_present_notify()` before rendering. Without it, back-to-back redraws on Wayland only arrive when something else wakes the loop, so keep it.

## SSH phase 3: files (done)

The user wanted Tabby-style SSH file transfer: "ZMODEM shouldn't be the only way to transfer". Later: "rsync is a good option if the server has it available, and add the possibility of ZMODEM. The most appropriate one should be used." What's built, all with the system's OpenSSH:

- **The engine** (`files/transfer.rs`): uploads and downloads for the panel, drag and drop, and `ionterm-get`/`ionterm-put`. Runs over the tab's shared connection (`ControlMaster`; `Remote` = `Connection::exec_argv`), one thread per transfer, at most 3 at once, cancellable.
  - **rsync** when both ends have it (`rsync::usable`: Linux only, local `rsync --version` once, remote `command -v rsync` cached per host). `-s` keeps paths away from shells, `--partial-dir=.ionterm-partial` resumes interrupted files, cancel = SIGTERM, then the partial is removed. The transfer card says "via rsync".
  - **Otherwise `cat`/`tar` over ssh**, with `.part` files, the `tar` crate on the local side ("via ssh").
- **The file panel** (`ctrl+shift+o`, task 1) and **drag and drop** (task 2) use the engine. Dropping on a typed-`ssh` pane (no shared connection) queues the files for the next `rz` instead ("Run rz on the server to receive …").
- **ZMODEM** (`zmodem/`): the PTY reader spots a valid ZRQINIT (`sz`) or ZRINIT (`rz`) hex header and hands the bytes to a `Session`; input to the pane is dropped while it runs (`Pane::write`), and ctrl+c or Escape cancels.
  - `sz file` downloads to the download folder (taken names become `file (1).ext`; the sender can't wait for a question).
  - `rz` sends files dropped on the pane beforehand, or opens the picker; picking writes a ZRQINIT to the PTY so `rz` answers at once instead of after its 10 s timeout.
  - Cancelling a download sends ZSKIP: `sz` stops and finishes normally, so nothing leaks onto the screen. lrzsz doesn't reliably acknowledge CANs, which is why an earlier design swallowed the prompt. CANs are only the fallback after 5 s.
  - ZCOMMAND (the server asking to run a command) is refused.
- **`ionterm-get <file|dir>…` and `ionterm-put [dir]`**: shell functions the bootstrap defines (bash, zsh, fish) that send OSC 5379 (`get`/`get-dir`/`put` + base64 absolute path). Only followed for connections ionterm opened. They work without lrzsz and use the engine above.

**Still for the user to check** (needs the mouse or pops a portal dialog): the upload picker and `rz`'s picker, the panel's mouse parts, dragging files onto local, SSH and typed-`ssh` panes, and the follow button.

## Next tasks (from docs/PLAN.md)

- **v0.3b power features:**
  - Scrollback search.
  - Cursor trail (background image done: `app/background.rs`, `Renderer::set_background`).
- **v0.2 leftovers:** kitty graphics animation (`a=f`, `a=a`, `a=c`), relative placements (`P`, `Q`).
- **v0.4:**
  - Upstream `extras/ionix-styler` into `/home/ion/development/ionix/profile/airootfs/usr/local/share/ionix/styler/`, then make ionterm the Ionix default terminal (replace kitty in Hyprland keybinds, ioexplorer/hypr-settings launcher lists, ionix-hyprtab's btop launcher).
  - Windows polish (own titlebar, installer).
  - `ionterm @` over a Windows named pipe (Unix socket done: `remote.rs`, `app/remote.rs`).
  - Before installing the terminfo locally, ship the planned `ssh` wrapper or shell integration that handles `TERM` for typed `ssh` (see "Settled decisions" above).
- **Partly verified on a real server:** connecting to `ionserver`, its bash and zsh hooks, the file panel and rsync work there. Reconnect after a dropped connection is still only tested against the fake-ssh setup.
