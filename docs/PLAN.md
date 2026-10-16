# ionterm: plan and architecture

A GPU-rendered terminal emulator in the spirit of kitty: fast, good inline graphics, useful built-in features, and themed by the Ionix styler like the rest of the Ionix desktop. Wayland first, and Windows compiles from day one.

## Decisions

| Area | Decision |
|---|---|
| Language | Rust (edition 2024), standard rustfmt style like ioexplorer |
| Windowing | `winit`: native Wayland (plus X11 for free) and Win32 |
| Rendering | `wgpu`: Vulkan on Linux, DX12 on Windows |
| Fonts | `cosmic-text` for discovery, fallback, shaping (ligatures) and rasterisation. Box-drawing, block and Powerline glyphs are drawn procedurally so they line up exactly |
| VT parsing | `vte` crate for the state machine; our own `Perform` implementation and grid |
| PTY | `portable-pty`: openpty on Unix, ConPTY on Windows |
| Config | TOML: `~/.config/ionterm/ionterm.toml` (user) + `theme.toml` (owned by the Ionix styler), live-reloaded |
| Graphics | Kitty graphics protocol (full, including unicode placeholders) and Sixel |
| Multiplexing | Built-in tabs, splits and layouts (kitty-style) |
| Chrome | Configurable: `decorations = "auto" \| "none" \| "full"`. `auto` = none on Wayland tiling, our own titlebar on Windows. Tab bar drawn by the GPU, hidden with a single tab |
| Keybindings | kitty-like defaults (`ctrl+shift+…`), fully remappable |
| Distribution | `ionterm-git` PKGBUILD / AUR; later the default terminal in the Ionix ISO |
| TERM | `xterm-ionterm` terminfo with a smart SSH fallback (see below) |

## Workspace layout

```
crates/
  ionterm-vt/       Terminal state: parser glue, grid, scrollback, modes, SGR, OSC,
                    kitty keyboard flags, graphics store. Pure logic, no I/O, no GPU.
  ionterm-render/   wgpu renderer: glyph atlas, cell/background/cursor pipelines,
                    procedural glyphs, image layers, tab bar.
  ionterm/          The app binary: winit event loop, windows/tabs/splits, PTY,
                    config + theme + live reload, input encoding, clipboard.
extras/
  terminfo/         ionterm.terminfo source (xterm-ionterm)
  shell-integration/ fish / bash / zsh scripts (OSC 133, OSC 7, ssh wrapper)
  ionix-styler/     theme templates + install functions to upstream into the Ionix repo
docs/
```

`ionterm-vt` stays independent of the renderer and the windowing layer, so it can be tested without a display and reused, for example by a headless `ionterm +replay` debugging tool.

## Runtime model

```
           ┌───────────── PTY reader thread (per pane) ─────────────┐
 child ──▶ │ read() → vte::Parser → Term (behind Mutex) → wake proxy │
           └────────────────────────────────────────────────────────┘
                                                      │ UserEvent::Wakeup
 winit main thread ◀──────────────────────────────────┘
   • input → encode → PTY writer
   • on redraw: lock Term, snapshot visible rows, unlock, render with wgpu
   • config watcher (notify) → UserEvent::ConfigChanged → re-theme
```

- Each pane owns a `Term` behind a `Mutex`. The reader thread parses straight into it and only signals the main thread, coalescing wakeups, so floods like `cat bigfile` don't queue thousands of redraws.
- Replies the terminal must send back (DSR, DA, XTGETTCAP, kitty queries) collect in `Term`'s outgoing buffer, which the reader thread writes to the PTY.
- Rendering snapshots only the visible rows under the lock, so the parser is never blocked by GPU work.

## Theming and Ionix integration

ionterm joins the Ionix styler the same way kitty and ioexplorer do:

1. Each theme gets a `ionterm/theme.toml` section: a static file, plus `adaptive/ionterm/theme.toml` with `{{slot}}` tokens for adaptive themes.
2. `install-default` gets `ionix_install_ionterm` / `ionix_uninstall_ionterm`, which symlink the file to `~/.config/ionterm/theme.toml`.
3. ionterm watches **the directory** `~/.config/ionterm/`, not the file, because the styler swaps the symlink rather than editing it. It reloads within about 250 ms, so no signal is needed.
4. With no `theme.toml`, ionterm falls back to built-in Ionix defaults (the `Theme.qml` palette).

`theme.toml` shape (the adaptive template maps the palette slots):

```toml
foreground = "{{textBright}}"
background = "{{bgDeep}}"
background_opacity = 0.92
cursor = "{{accent}}"
cursor_text = "{{bgDeep}}"
selection_foreground = "{{bgDeep}}"
selection_background = "{{accent}}"
url = "{{accentBright}}"

[tab_bar]
background = "{{bgDeep}}"
active_foreground = "{{bgDeep}}"
active_background = "{{accent}}"
inactive_foreground = "{{text}}"
inactive_background = "{{bgWindow}}"

[palette]
colors = ["{{ansi0}}", "{{ansi1}}", …, "{{ansi15}}"]
```

The templates and install functions start in `extras/ionix-styler/` and move into the Ionix repo when ionterm becomes the default terminal.

## TERM, terminfo and SSH

kitty's "unknown terminal: xterm-kitty" happens because `TERM` is forwarded over SSH but the remote host has no terminfo entry for it. ionterm handles that in layers:

1. **Locally**, `TERM=xterm-ionterm` if the entry is installed (the package ships it to `/usr/share/terminfo`), otherwise `xterm-256color`. It also sets `COLORTERM=truecolor`, `TERM_PROGRAM=ionterm` and `TERM_PROGRAM_VERSION`.
2. **Over SSH**, the shell integration wraps `ssh`. On the first connection to a host it tries `infocmp -x xterm-ionterm | ssh host 'tic -x -'` and caches whether that worked in `~/.cache/ionterm/ssh-terminfo`. If it worked, TERM stays `xterm-ionterm`. Otherwise the session uses `TERM=xterm-256color`.
3. **Feature detection doesn't depend on TERM.** ionterm answers XTGETTCAP, DA1/DA2/XTVERSION, the kitty keyboard query (`CSI ? u`) and kitty graphics queries. Modern apps (neovim, yazi, helix) probe these directly, so nothing is lost even under `xterm-256color`.
4. The `xterm-ionterm` entry builds on `xterm-256color` and adds truecolor (`Tc`/`RGB`), styled and coloured underlines (`Smulx`, `Setulc`), synchronized output (`Sync`), bracketed paste, focus events, cursor shapes (`Ss`/`Se`) and OSC 52 (`Ms`).

## Configuration (`ionterm.toml`)

```toml
[font]
family = "JetBrainsMono Nerd Font"
size = 11.0
ligatures = true

[window]
padding = 8
decorations = "auto"          # auto | none | full
opacity = 0.92                # overrides theme when set

[terminal]
scrollback_lines = 10000
shell = ""                    # empty = $SHELL / passwd entry; cmd/pwsh on Windows

[cursor]
shape = "beam"                # block | beam | underline
blink_interval = 0.5          # seconds, 0 disables

[keybindings]
"ctrl+shift+c" = "copy"
"ctrl+shift+v" = "paste"
"ctrl+shift+t" = "new_tab"
"ctrl+shift+enter" = "split"
"ctrl+shift+e" = "hints"
```

Every key has a default, so ionterm runs with no config at all.

## Roadmap

### v0.1: daily-drivable core
- [x] Workspace, CI (Linux + Windows build), PKGBUILD skeleton
- [x] winit window + wgpu surface with transparency
- [x] PTY spawn (portable-pty), reader thread, resize (TIOCSWINSZ / ConPTY resize)
- [x] VT core: cursor movement, erase, insert/delete, scroll regions, SGR (256 + truecolor), alternate screen, DEC modes, tabs, OSC 0/2 titles
- [x] Grid renderer: glyph atlas, backgrounds, cursor shapes, bold/italic, underline styles, strikethrough
- [x] TOML config + theme.toml with built-in Ionix defaults
- [x] Ligatures (run-based shaping) and procedural box-drawing / block / Powerline glyphs
- [x] Scrollback viewing (mouse wheel, shift+pgup/pgdn)
- [x] Selection (mouse, word/line), clipboard (Wayland data-control + primary)
- [x] Mouse reporting (X10/normal/button/any, SGR 1006, urxvt)
- [x] Bracketed paste, focus events, synchronized output (mode 2026)
- [x] Kitty keyboard protocol (all flags)
- [x] Live reload of config + theme (directory watch)
- [x] OSC 4/10/11/12/104/110–112 colour set and query, OSC 52 clipboard (write only)
- [x] Device replies: DA1/DA2, DSR/CPR, XTVERSION, XTGETTCAP, DECRQM, DECRQSS
- [x] xterm-ionterm terminfo source (installed in v0.3 together with the SSH wrapper)
- [x] Linear-light text blending and unhinted glyphs, matching kitty's rendering
- [x] OSC 8 hyperlinks and plain-text URL detection: underline on hover, ctrl+click to open (ctrl+shift+click inside mouse-grabbing apps)
- [x] ctrl+click file paths: with ctrl held, paths that exist (and `file:line:col`) in local panes underline and open like hints do (editor at the line, folders in the file manager)
- [x] Reflow on resize (primary screen and scrollback; the alternate screen is left to the app)
- [x] OSC 133 prompt marks: while the shell is at its prompt, resizing blanks the prompt instead of reflowing it, so shells that redraw (fish, oh-my-posh) don't leave stray copies
- [x] Kitty keyboard protocol verified with real keystrokes (`scripts/keytest.sh`, Hyprland)
- [x] Selection auto-scroll while dragging past the edge

### v0.2: multiplexing and graphics
- [x] Tabs: Windows Terminal-style bar at the top (shown with 2+ tabs), close and new-tab buttons, drag to reorder, double-click or ctrl+shift+alt+t to rename, right-click menu (rename, duplicate, colour, close, close others), foreground-process icons; kitty Powerline styles as an option
- [x] Splits and layouts: splits, tall, fat, grid, stack, zoom; directional focus, resizing, moving panes; new tabs and splits start in the current directory
- [x] Drag split borders with the mouse (the pointer turns into a resize arrow over them)
- [x] Kitty graphics protocol: transmission (direct and chunked, file, temp file, shared memory; RGB, RGBA, PNG, zlib), queries and replies, placements anchored to their rows (scroll, scrollback, reflow), z-index above/below text, deletion, unicode placeholders (tmux)
- [ ] Kitty graphics animation (`a=f`, `a=a`, `a=c`) and relative placements (`P`, `Q`)
- [x] Sixel: streaming decoder (RGB/HLS colours, repeats, raster attributes, transparent or filled background), shown through the same image path as kitty graphics; DECSDM (mode 80), XTSMGRAPHICS; announced in DA1 only with `advertise_sixel`

### v0.3: first-class SSH (Tabby-style)

Engine: the system's OpenSSH (`ssh`, `sftp`), so the user's whole ssh config, agent and keys apply. On Unix, connections share one master connection (`ControlMaster`), so new tabs and file transfers don't log in again. Windows' OpenSSH lacks connection sharing; there, each one connects on its own.

Phase 1: connecting
- [x] Hosts from `~/.ssh/config` (following `Include`), with ionterm extras (colour, icon, group) in `~/.config/ionterm/hosts.toml`
- [x] "Connect SSH" palette (`ctrl+shift+s`): fuzzy search, Enter connects in a new tab
- [x] `⌄` button next to `+` in the tab bar, opening the same picker
- [x] `ionterm host <host>` on the command line
- [x] `ionterm ssh <ssh args>` (`ionterm ssh -J jump user@host -p 2222`): any connection, with the same bootstrap, reconnect and file panel as config hosts. The given options go to every command sharing the connection, minus the session-only ones (tty, forwarding)
- [x] `host` and `ssh` open a tab in the running ionterm (`IONTERM_SOCKET`, else the newest window; also `ionterm @ host|ssh`), or a new window when none runs or with `-w`/`--new-window`
- [x] Remote bootstrap on connect: install the `xterm-ionterm` terminfo (or fall back to `xterm-256color`), and a session-only OSC 7 hook for fish/bash/zsh so ionterm knows the remote directory. Nothing else is written to the server.
- [x] "Duplicate tab" on an SSH tab connects to the same host (new tabs and splits stay local)
- [x] Disconnect banner with Enter to reconnect, instead of closing the tab
- [x] Per-host tab colour and icon
- [x] Typed `ssh` detection: SSH icon and host in the tab, duplicate re-runs the command

Phase 2: managing connections
- [x] "New connection…" / edit form (`ctrl+e` in the picker): alias, host, user, port, key, jump host, colour, icon, group. Saved to `~/.ssh/config.d/ionterm` (hand-added lines kept), with one `Include` added to the top of `~/.ssh/config` (backed up once to `config.ionterm-backup`). Hosts from the user's own config can get a colour, icon and group, but their ssh settings stay read-only. Delete with a confirm click.

Phase 3: files
- [x] File panel (`ctrl+shift+o` or "Files…" in the tab menu): browse over the shared connection, follow the shell's directory, type to filter, upload (system file picker), download to Downloads, rename, delete, new folder, progress with cancel, Overwrite / Rename / Skip on name conflicts. Only for connections ionterm opened.
- [x] Drag and drop: files dropped on an SSH pane upload to the shell's remote directory (or the folder open in the file panel, when dropped there); on a local pane their quoted paths are typed in. Wayland drops come through ionterm's own `wl_data_device` (winit 0.30 has none)
- [x] Transfers use rsync when both ends have it (resumes, sends only changes), else `cat`/`tar` over the shared connection
- [x] ZMODEM: `sz` and `rz` in a pane are handled natively (no local lrzsz needed), with progress and clean cancelling; files dropped on a typed-`ssh` pane go to the next `rz`
- [x] `ionterm-get <file>` / `ionterm-put [dir]` on the server (installed per session by the bootstrap): download or upload without lrzsz

### v0.3b: other power features
- [x] Shell integration (OSC 133): bash, zsh and fish 3 are set up automatically, locally and over ionterm's SSH (fish 4 marks prompts itself). Jump between prompts (ctrl+shift+z / x), copy the last command's output (ctrl+shift+g), ctrl+shift+right-click selects a command's output, a red mark beside prompts whose command failed, and clicks on the command line move the cursor (fish's `click_events`)
- [x] Command info on hover: prompt rows keep their command's run time and end time (OSC 133;C → D); hovering one shows "✗ exit 1 · took 2.5s · 5m ago" at the end of the line
- [x] Progress in the tab: OSC 9;4 (cargo, systemd, winget…), or a percentage on a line a command keeps rewriting (curl, wget, pacman)
- SSH wrapper with terminfo install and fallback
- [x] Settings tab (the gear at the end of the tab bar, ctrl+, or ctrl+shift+f2): switches, choices, steppers, text fields and a font picker with previews for the simple settings, saved instantly to `ionterm.toml` with `toml_edit` (comments kept; a setting back at its default is removed); `ionterm.toml`, `theme.toml` and the SSH config open in `$EDITOR` inside the tab, which returns to the page when the editor exits
- [x] Hints mode (ctrl+shift+e): URLs (and OSC 8 links), file paths, `file:line[:col]`, git hashes and IPs get home-row labels; typing one opens it (files in `$EDITOR` at the line in a new tab, or the `[hints] open_file` command), shift copies, alt types it into the command line. Paths in SSH panes are copied
- [x] Multi-line paste guard: pasting several lines into a program without bracketed paste asks first (Paste / As one line / Cancel); `[terminal] confirm_multiline_paste`
- [x] Tab activity and notifications: background tabs show new output (dot), bell, and ✓/✗ for long commands; a desktop notification (`notify-send`) when a command that ran `[notifications] command_done_after` seconds (10) ends while you're elsewhere; OSC 9 and OSC 777 notifications from programs, at most one a second
- [x] Command palette (ctrl+shift+p): every action with its keybinding, fuzzy-searched, plus SSH hosts to connect to (`commands.rs`, reusing the picker in `palette.rs`)
- [x] Scrollback search (ctrl+shift+f): plain or regex (alt+r), smart case, across soft wraps, every match highlighted, Enter/shift+Enter or arrows to step, re-searched as output arrives. Moving panes moved to ctrl+shift+alt+f / b
- [x] Scrollback in a pager (ctrl+shift+h, kitty's `show_scrollback`): the whole history with colours, wrapped lines joined, opened in `less -R` in a tab at the line on screen; `[terminal] scrollback_pager` with `INPUT_LINE_NUMBER`
- [x] Session restore (`[session] restore`, off by default): tabs, split trees with their ratios, layouts, folders, SSH hosts, tab titles and colours, saved every few seconds when they change and on window close (`$XDG_STATE_HOME/ionterm/session.toml`). Closing the last tab ends the session; with several windows the first owns it. Scrollback isn't kept
- [x] Clear scrollback (ctrl+shift+k: the scrollback and everything above the prompt being typed, which moves to the top with its marks), copy the current directory, open it in the file manager (palette)
- [x] Broadcast input (ctrl+shift+b): typing and pasting go to every pane of the tab, each key encoded for that pane's modes; yellow borders and a broadcast tab icon while on
- [x] Background image (`[window] background_image`, PNG/JPEG/WebP, cover-fitted, `background_image_opacity`; reloads live, errors in a toast)
- [x] Cursor trail (`[cursor] trail`, off by default): jumps of more than two cells leave a tapering streak that eases into the cursor over 160 ms (`app/trail.rs`)

### v0.4: Ionix and Windows
- Styler section upstreamed into the Ionix repo; ionterm replaces kitty in the ISO, Hyprland keybinds, ioexplorer/hypr-settings launcher lists, ionix-hyprtab
- Windows polish: own titlebar, DirectWrite font fallback check, installer/zip release
- [x] `ionterm @` remote control over a Unix socket (`ls`, `send-text`, `get-text`, `new-tab`, `split`, `focus`, `close`, `set-title`, `action`); `IONTERM_SOCKET` / `IONTERM_PANE` in every pane, stale sockets cleaned up. Windows named pipe still to do

### v0.5: workflow

Picked from a brainstorm of what would make ionterm the ideal terminal for dev work and server management. Each item lists the open design questions to ask before starting it.

**Tabs between windows**

Today every window is its own process with its own socket (`ionterm-<pid>.sock`), so a tab can't simply change windows.
- [x] One process for every window (kitty's single instance, `[terminal] single_instance`): `ionterm` asks a running ionterm that is the same program (`ionterm @ version` names its path) to open the window (`new-window`, with the directory and activation token) and exits; `--standalone` opts out. Alternative considered: passing the PTY fd between processes (`SCM_RIGHTS`), but the shell's parent would still be the old process, so its exit code is lost.
- [x] `App::window` is the window being worked on and `App::parked` the others; every event selects its window first (`app/windows.rs`), so the rest of the app still deals with one window. Panes, transfers (ids now global) and file drops (one Wayland listener, events tagged with their surface) are routed to their window
- [x] New window (ctrl+shift+n, `new_os_window`), in the active pane's directory
- [x] Drag a tab out of the tab bar (48 px past it or the window's sides) and let go: it opens in a window of its own, or moves into the ionterm window the pointer lands on (at the tab bar slot under it)
- [x] Without the mouse: "Move to new window" and "Move to window…" in the tab menu, the palette and as actions; `ionterm @ move-tab (--new-window | --window N)`
- [x] Single panes too: "Move pane to…" picks a new tab, a new window or any other tab (in any window) to split into; `move_pane_to_new_tab`, `move_pane_to_new_window`; `ionterm @ move-pane (--new-tab | --new-window | --tab N [--window N])`
- [x] Panes keep their shell, scrollback, images, SSH connection and transfers when they move; a tab keeps its file panel (a pane moving alone closes the panel it browsed with)
- [x] Session restore saves every window (`[[windows]]` in `session.toml`; older one-window files still load). Windows opened for one thing (`-e`, `host`, `ssh`) aren't saved, and the first plain window an ionterm opens is the one that restores
- [x] `ionterm @ ls` has a window column; `new-window [--cwd] [--title] [--host H | --ssh ARGS… | command…]`; `version`

**Edit remote files locally**

- [x] `ionterm-edit <file>` on the server: a script (not a function, so it works as `$EDITOR` for `git commit`/`crontab -e`) that the bootstrap puts in a session temp folder on `PATH` (removed when the shell exits). It sends OSC 5379 `edit;<base64 path>;<nonce>` and waits, echo off, for ionterm to type back `<nonce> saved|unchanged|<error>`. Outside ionterm (tmux, another terminal) it runs `${IONTERM_EDIT_FALLBACK:-vi}`. ctrl+c stops waiting; ionterm then doesn't type the answer at the prompt (shell integration tells)
- [x] ionterm (`edit.rs`, `app/editing.rs`) copies the file to a private folder in the runtime dir (0700/0600), opens it with `$VISUAL`/`$EDITOR` in a tab next to the SSH tab ("✎ host:name"), watches the folder and writes it back 200 ms after each save (skipped when the content didn't change). A graphical editor gets the file without waiting
- [x] Writes go to a temp file on the server, are checked for length, then copied over the real file in place (`cat > file`), keeping owner, mode and hard links. A file changed on the server since (mtime and size) asks: Overwrite / Show changes (a `diff -u` tab) / Don't save
- [x] Files the login user can't read or write: asks "Edit with sudo?", then for the password if sudo wants one (masked field; kept in memory only while the file is open, `sudo -S` with the password first on stdin)
- [x] "Edit" in the file panel: Enter on files that look like text (small, not executable, no binary extension), ctrl+e and the row menu; ctrl+d still downloads
- [x] The local copy goes when the editor quits or its tab is closed, and all of them when ionterm exits
- Only for connections ionterm opened, like the other OSC 5379 commands

**Port forwards from the tab**

- [x] "Forward a port…" (`forward_port`, the palette, SSH tabs' right-click menu): the server's listening ports (`ss -ltnp`, else `/proc/net/tcp`) with their programs, plus typing one in either way. Added with `ssh -O forward` on the shared master (no new login) and removed with `ssh -O cancel`; on Windows (no master) each is an `ssh -N` (`forward.rs`, `app/forwards.rs`)
- [x] Specs: `3000`, `8080:80`, `8080:db.internal:5432`, `R 9000[:3000]` for the server listening
- [x] Each pane owns its forwards (`Pane::forwards`) and cancels them when it closes (the master outlives the tab by `ControlPersist`); they move with the pane between windows
- [x] Chips at the right of the tab bar (they show the bar even with one tab): forwards and the ports a local pane's processes listen on (`/proc/net/tcp*` inodes against the pane's process tree's fds, every 2 s for the visible tab). Click opens `http://localhost:<port>`, right/middle click stops a forward, hovering explains it
- [x] A local port that's taken moves to the next free one, and one below 1024 (root only) moves up by 8000 (443 → 8443); the toast says why
- [x] Saved per host (`forwards = […]` in `hosts.toml`, "Port forwards" in the connection editor): set up once the master is up (`ssh -O check`, up to 2 minutes for a password), and again after a reconnect
- [x] `ionterm @ forward SPEC` (waits, prints the local port), `unforward [R ]PORT`, `forwards`

**Command blocks you can act on**

Builds on the OSC 133 marks. Rule for all of it: **regular terminal use doesn't change.** Clicking, dragging, double/triple-click selection, mouse reporting and scrolling work exactly as they do now; nothing covers the text and nothing opens by itself.
- [x] Command lines are known per prompt (`Row::command`): the shell's `cmdline` (zsh, fish 4, and now fish 3's integration), or what was typed after the prompt's end mark (OSC 133;B, which bash's integration now adds to `PS1`). Also improves "finished" notifications for bash
- [x] Sticky command header (`[terminal] sticky_command`, a settings toggle, on by default): while scrolled into a command's output, or while it runs with its prompt off screen, its command line is pinned over the pane's top row with ✓/✗/❯ and "took 2.5s · 5m ago" or "running 1m 03s". Click it to jump to the prompt. Never in the alternate screen or when idle at the bottom (`blocks.rs`, `app/blocks.rs`)
- [x] Scrollbar (`[terminal] scrollbar`, a settings toggle): in the right padding only (clicks on text still select), shown for 1.2 s after scrolling or while the mouse is in the padding; marks for prompts, failed commands (red) and search hits; click or drag to jump
- [x] Block handle, never in the way: hovering a command's block shows three dots in the left padding by its first visible row and a faint line along it (not while selecting or dragging, not in mouse-reporting apps or the alternate screen). Clicking the dots opens the block menu: copy command, copy output, copy both as Markdown, run again, open the output in the editor or the pager, save it to Downloads, fold/unfold
- [x] The same from the keyboard and the palette: `copy_command`, `copy_command_markdown`, `rerun_command`, `edit_command_output`, `page_command_output`, `save_command_output`, `fold_output`, `command_menu` (arrows and Enter), on the block at the top of the view when scrolled back, else the last command
- [x] Run again: typed at the prompt when the shell waits there (clearing a half-typed line first), else in a new split beside it (same directory, or the same host for SSH)
- [x] Folding (`Term::toggle_fold`): a finished command's output keeps its first and last 3 lines, the rest of what's in the scrollback becomes one "⋯ 4,812 lines folded" line; click it to unfold. The screen itself never folds, so the cursor and the bottom of the view behave as always. Every viewport↔line mapping goes through the folds (`viewport_to_absolute`, `absolute_to_viewport`), so selection, copying, search (a match inside a fold unfolds it), hints and images still line up. Folds survive reflow (the mark is on the prompt row) and go with their prompt when the scrollback is trimmed or cleared
