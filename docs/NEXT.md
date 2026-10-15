# ionterm: handoff for the next session

Start a fresh session in `/home/ion/development/ionterm` with something like:

> Read `docs/NEXT.md`, then do task 1.

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
  pty.rs                 portable-pty spawn (OutputSink), reader thread, exit_code, foreground process
  input.rs               key encoding (legacy + kitty), mouse encoding, KeyCombo, Action enum, DEFAULT_BINDINGS
  config.rs / theme.rs / links.rs
extras/                  terminfo source, Ionix styler theme files + install functions, example ionterm.toml
scripts/keytest.sh       real-keystroke test of the keyboard protocol (Hyprland)
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
- **System and shell:** Arch, Hyprland 0.56 (Lua config, so `hyprctl dispatch` takes Lua), fish 4.9 with an oh-my-posh prompt that has a right-hand block, kitty with JetBrainsMono Nerd Font 11pt, Intel GPU on Vulkan.
- **SSH:** 51 hosts in `~/.ssh/config`, no Include before ionterm adds one. Key login to localhost does NOT work, and localhost's host key changed in `known_hosts`.
- **Tools installed:** `kitten`, `chafa`, `yazi`, ImageMagick `magick`, `sftp`, `scp`. Not installed: `lrzsz`.

**Driving ionterm through Hyprland: always use `scripts/drive.sh`.** Claude Code runs inside the user's **kitty**, which is normally the focused window. A dispatch with an empty or stale pid (`window = "pid:"`) can land on kitty, and test keys like ctrl+d, Escape or Return then hit Claude Code itself. A previous session lost its own terminal this way. `drive.sh` only sends keys to, or kills, the window it launched, after checking that it is a live `ionterm` window and not an ancestor of the shell. Never use `pkill`/`killall`, `killactive`, or a dispatch without a verified `window =`.
```bash
scripts/drive.sh launch "/home/ion/development/ionterm/target/release/ionterm"   # the user's real env; waits for the new window, floats it, 1200x650
scripts/drive.sh key "CTRL SHIFT" t     # to that window only; aborts (exit 9) if it is gone
scripts/drive.sh shot shot.png          # then Read the png to look at it
scripts/drive.sh close
```
- **Key names** are xkb names (`Return`, `BackSpace`, `period`, `Down`, …). `slash` did not type a `/`, so avoid needing one.
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
  Launch with `hl.dsp.exec_cmd("env PATH=$SCRATCH/bin:/usr/bin …/ionterm ssh fakehost")`. Don't connect to the user's real servers without asking: the bootstrap writes `~/.terminfo` there.
- **Debugging and timing:**
  - `IONTERM_LOG=ionterm_vt=debug` logs unhandled escape sequences.
  - `IONTERM_DUMP=<file>` records raw PTY output.
  - For animations, capture `grim` frames ~50 ms apart and measure pixels with PIL.
- **Wayland redraws:** `app.rs` calls `window.pre_present_notify()` before rendering. Without it, back-to-back redraws on Wayland only arrive when something else wakes the loop, so keep it.

## Next tasks: SSH phase 3, files (do these first)

The user wants **Tabby-style SSH file transfer**. Their words: "similar to Tabby's SSH support, having a file upload/download feature and such to be usable", and "ZMODEM shouldn't be the only way to transfer. It can be the default, but have a fallback if not available on the server."

They already chose:
- an SFTP side panel;
- drag-and-drop upload;
- ZMODEM with a fallback;
- system OpenSSH as the engine (not an embedded SSH library);
- remote shell integration for the remote directory (already built: OSC 7 → `Pane::remote_cwd`).

Decided on 2026-10-15:
- **Panel only for ionterm-launched connections.** Typed `ssh` sessions (`pane.detected_ssh`) don't get the panel. Show a hint like "Reconnect via ionterm to browse files" instead.
- **Upload picker: the XDG portal** (`org.freedesktop.portal.FileChooser` via `ashpd`/`zbus`), with a native dialog on Windows (e.g. `rfd`). No path-input field first.
- **Download name conflicts: ask each time** with Overwrite / Rename / Skip (Rename = `file (1).txt`).
- **Add `ionterm-get <file>` and `ionterm-put [dir]`:** the bootstrap installs these shell functions on the server. They trigger a download, or open the picker and upload, through an OSC sequence, so lrzsz isn't needed. They are part of task 3.

### Shared groundwork: a transfer engine over the existing connection

Panes opened by ionterm have `pane.connection: Option<ssh::Connection>`, whose argv carries `-o ControlMaster=auto -o ControlPath=~/.cache/ionterm/ssh/%C -o ControlPersist=300` on Unix. Extra `ssh` processes using the same ControlPath and alias reuse the open connection instantly, with no second login. Windows' OpenSSH has no ControlMaster, so there every operation authenticates on its own (fine with keys or an agent).

Suggested approach (confirm with the user if unsure): run single commands over that shared connection instead of parsing interactive `sftp` output.
- **List:** `ssh <ctl-opts> <alias> -- 'cd -- <dir> && LC_ALL=C ls -la --time-style=+%s'`. GNU `find -printf` is nicer, but BusyBox lacks it, so parse `ls -la` with a fallback. Show size, mtime, type, permissions, symlink targets.
- **Download:** `ssh … -- cat -- '<path>'`. Read stdout in a thread, write to `~/Downloads` (`dirs::download_dir()`) via a `.part` file, and count bytes for progress (size known from the listing).
- **Upload:** `ssh … -- "cat > '<path>.part' && mv -- '<path>.part' '<path>'"`, writing the local file to stdin.
- **Directories:** stream `tar`: `tar -C <dir> -cf - <name> | ssh … tar -C <dest> -xf -`, and the reverse.
- **Rename / delete / mkdir:** `mv`, `rm -r` (confirm first), `mkdir -p`.
- **Remote paths:** always single-quote them, escaping `'` as `'\''`. Add unit tests for the quoting.
- **Threading:** run everything off the UI thread. Report progress through new `UserEvent` variants (e.g. `UserEvent::Transfer(id, TransferEvent)`) and keep a transfers list on `TermWindow`. Let transfers be cancelled (kill the child).
- **Typed `ssh`:** typed sessions (`pane.detected_ssh`) have no ControlMaster. For them, either offer the panel only for ionterm-launched connections, or run commands with the same argv (prompting for auth in a hidden PTY is messy). Ask the user which they prefer.

### Task 1: SFTP side panel (done)

Built as described below, and tested against fake ssh: listing, follow terminal, filter, download (file and folder), the conflict dialog, new folder and delete. **The user still needs to check** the upload picker (a portal dialog would pop up on their screen) and the mouse parts (clicks, double-click, right-click row menu, hover, the toolbar buttons). Hooks for the next tasks: `App::upload(pane, remote, dir, paths)` uploads with conflict handling, so drag and drop (task 2) can call it. `ionterm-get` (task 3) needs a "download one remote path" helper, which can build an `Entry` and call `queue_downloads`.

The original brief:

- **Opening it:** a panel on the right of an SSH tab, toggled by a new action. Suggested key `ctrl+shift+o` (`ctrl+shift+f` is taken by `move_pane_forward`; `ctrl+shift+e` is reserved for hints). Also add "Files…" to the tab right-click menu (`menu.rs`) for SSH tabs.
- **Building it:** treat it as a widget like `palette.rs`/`editor.rs` (`layout`/`hit`/`draw` from `ChromeItem`s), but docked rather than an overlay. It should take width from the tab's pane area in `TermWindow::relayout` (the panes get a narrower area).
- **Browsing:** a breadcrumb path, list rows (folders first, Nerd Font icons via `tabbar::process_icon`-style mapping by extension), keyboard navigation (Enter opens a folder, Backspace goes up), mouse double-click, and a "follow terminal" toggle that tracks `pane.remote_cwd` (OSC 7).
- **Actions:** download (to `~/Downloads`, then "show in folder" via `xdg-open`/ioexplorer), upload (a local file picker: XDG portal `org.freedesktop.portal.FileChooser` via `ashpd` or `zbus`, or a simple path-input field first), rename, delete (confirm), new folder, refresh.
- **Transfers:** a strip at the bottom of the panel with progress bars, speed, and cancel.

### Task 2: Drag and drop upload

- **The catch:** winit 0.30 does not support file drops on Wayland (`WindowEvent::DroppedFile` is only emitted on X11, Windows and macOS; verified in `winit-0.30.13/src/platform_impl`). On Wayland this needs its own `wl_data_device` handling:
  - Use `smithay-client-toolkit`'s data-device/DnD support on the window's `wl_surface`, with the display and surface from `window.display_handle()` / `window.window_handle()` (raw-window-handle) on a separate event queue.
  - Accept the `text/uri-list` mime type, and parse `file://` URIs (percent-decoded; `pane::path_from_file_url` exists).
  - Check the winit version first: newer winit may support it, and upgrading may be simpler.
- **Behaviour:** dropping files on an SSH pane uploads them to `pane.remote_cwd` (fallback: remote home), with progress in the panel's transfer strip (or a small toast if the panel is closed). Dropping on a local pane should paste the shell-quoted paths, like most terminals do.

### Task 3: ZMODEM, with SFTP as the fallback

- **Detecting it:** watch PTY output in `pty.rs`'s reader for the ZMODEM start sequences:
  - `**\x18B00` (ZRQINIT, from `sz` on the remote = download);
  - `**\x18B01` (ZRINIT, from `rz` = upload).
- **Handling it:** while a transfer runs, the reader hands bytes to a ZMODEM state machine instead of the `Processor`, and writes protocol replies to the PTY.
- **Implement it natively in Rust:**
  - `lrzsz` isn't installed locally and doesn't exist on Windows. Consider the `zmodem2` crate (check it's maintained and the license is OK) or a small ZMODEM receive/send implementation.
  - Downloads go to `~/Downloads` with progress. For `rz`, ask which local files to send (same picker as task 1) and allow cancelling (send ZABORT/CAN×5).
- **Fallback:** if the server has no `sz`/`rz`, uploads and downloads go through the SFTP-panel engine. The user explicitly wants ZMODEM as the default but never the only way. A simple helper command for the remote shell (e.g. a shell function `ionterm-get <file>` that the bootstrap defines, triggering a download through an OSC sequence) is a nice optional extra; ask first.

## Later tasks (from docs/PLAN.md)

- **v0.3b power features:**
  - Shell integration: jump between prompts (OSC 133 marks already tracked in `Row::prompt`), select a command's output.
  - Hints mode (`ctrl+shift+e`): pick URLs, paths and hashes with the keyboard.
  - Scrollback search.
  - Background image; cursor trail.
- **v0.2 leftovers:** kitty graphics animation (`a=f`, `a=a`, `a=c`), relative placements (`P`, `Q`), dragging split borders with the mouse.
- **v0.4:**
  - Upstream `extras/ionix-styler` into `/home/ion/development/ionix/profile/airootfs/usr/local/share/ionix/styler/`, then make ionterm the Ionix default terminal (replace kitty in Hyprland keybinds, ioexplorer/hypr-settings launcher lists, ionix-hyprtab's btop launcher).
  - Windows polish (own titlebar, installer).
  - Optional `ionterm @` remote control.
  - Before installing the terminfo locally, ship the planned `ssh` wrapper or shell integration that handles `TERM` for typed `ssh` (see "Settled decisions" above).
- **Not yet verified on a real server:** SSH connect, terminfo install and reconnect have only been tested against the fake-ssh setup. Ask the user how their real hosts behaved.
