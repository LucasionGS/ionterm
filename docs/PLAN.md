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
- [ ] One process for every window (kitty's single instance): `ionterm` hands off to the running instance over the socket and opens a window there, unless `--new-window`/`--standalone` asks for a separate process. Also makes new windows start faster and simplifies session ownership. Alternative considered: passing the PTY fd between processes (`SCM_RIGHTS`), but the shell's parent would still be the old process, so its exit code is lost.
- [ ] Drag a tab out of the tab bar to make it a window of its own; drop it on another window's tab bar to move it there (with the drag-to-reorder animation on the target)
- [ ] Without the mouse: "Move tab to new window" and "Move tab to window…" (a picker listing windows by their active tab) in the tab menu and the palette; `ionterm @ move-tab --to <window>|--new-window`
- [ ] Panes keep their scrollback, images, SSH connection, file panel and transfers when they move
- Open questions: should a single pane be able to move into another window as a split, or only whole tabs? Should session restore save several windows?

**Edit remote files locally**

- [ ] `ionterm-edit <file>…` on the server (another bootstrap function next to `ionterm-get`/`ionterm-put`, sent as OSC 5379 `edit`): download the file over the shared connection to a private temp folder, open it in the local editor (`[hints] open_file` / `$EDITOR` in a new tab or split), and upload it again every time it's saved (watched with `notify`)
- [ ] "Edit" in the file panel (Enter or double-click on a text file, the right-click menu)
- [ ] Root-owned files: write back through `sudo` (`sudo tee` over the connection, password asked in a dialog when needed, never stored)
- [ ] Safety: before uploading, check the remote file's mtime and size still match the copy that was downloaded, and ask (Overwrite / Show diff / Cancel) when someone else changed it. Keep the owner and mode. A toast or the transfer card says "Saved to host:/path"
- [ ] The temp copy is removed when the editor closes, the tab closes or ionterm exits
- Only for connections ionterm opened, like the other OSC 5379 commands
- Open questions: editor inside ionterm (new tab or split) or an external editor (VS Code)? Wait for the editor (blocking, so `ionterm-edit` works as `$EDITOR` for `git commit`/`crontab -e` on the server) or return at once?

**Port forwards from the tab**

- [ ] "Forward port…" in the palette and the tab menu of SSH tabs: remote port (or `host:port` behind the server) → local port, added with `ssh -O forward -L` on the shared ControlMaster, so there's no new login. Removed with `ssh -O cancel`. Reverse forwards (`-R`) too
- [ ] A chip in the tab bar for each live forward (`:3000 → :3000`); click opens `http://localhost:<port>`, right-click closes it
- [ ] Remote ports to pick from: listening ports on the server (`ss -ltnH`, falling back to `/proc/net/tcp`), with the process name when readable
- [ ] Saved per host in `hosts.toml` (`forwards = ["3000", "8080:localhost:80"]`), set up again on connect and after a reconnect; editable in the connection editor
- [ ] Local panes: ports the pane's process tree listens on (from `/proc/<pid>/net/tcp` and its fds) as a chip with an open button, so `npm run dev` shows `:5173`
- [ ] A local port that's already taken picks the next free one and says so
- Windows' OpenSSH has no ControlMaster, so there a forward runs its own `ssh -N -L` process
- `ionterm @ forward`/`unforward` for scripts

**Command blocks you can act on**

Builds on the OSC 133 marks. Rule for all of it: **regular terminal use doesn't change.** Clicking, dragging, double/triple-click selection, mouse reporting and scrolling work exactly as they do now; nothing covers the text and nothing opens by itself.
- [ ] Sticky command header: while scrolled into a command's output, its command line stays pinned at the top of the pane (one row, theme colours, with the ✓/✗, duration and time like the hover info). Clicking it jumps to the prompt. **A toggle in the settings page** (`[terminal] sticky_command`), off in the alternate screen and while the shell is at its prompt
- [ ] Scrollbar with markers: a thin overlay scrollbar shown while scrolling or hovering the right edge, with marks for prompts, failed commands and search hits; click or drag to jump. Also a setting
- [ ] Block actions, never in the way: a small `⋯` appears in the right padding beside a prompt only while the mouse is over that command's block (not during a drag, a selection or in mouse-reporting apps). It opens a menu: copy command, copy output, copy both as a Markdown code block, rerun, open the output in `$EDITOR` / the pager / a viewer like `jless`, save the output to a file
- [ ] The same actions without the mouse: palette entries and optional keybindings that work on the block at the cursor, or the last block (alongside the existing ctrl+shift+g / ctrl+shift+right-click)
- [ ] Folding: collapse a long output to its first and last few lines with a "… 4,812 lines" row; a fold marker in the left padding (next to the failed mark) and a palette/keybinding action. Copying, searching and the pager still see everything
- Open questions: should folds survive reflow and clear scrollback? Where should rerun go when the shell is busy (new split or refuse)?
