# ionterm

A fast, GPU-rendered terminal for the Ionix desktop. It picks up your wallpaper colours, draws its box lines without gaps, and has room for kitty-style tricks.

![ionterm showing box drawing, Powerline segments, ligatures and emoji](docs/screenshot.png)
<sup>The Ionix theme, mid-demo. Yes, the box lines actually touch.</sup>

## Key Features

- **GPU rendering**: everything is drawn with wgpu (Vulkan on Linux, DX12 on Windows), so scrolling through a huge log doesn't heat your lap.
- **Ionix theming**: plugs into the Ionix styler like kitty and ioexplorer do, wallpaper-adaptive colours included. Switch themes and the open terminal recolours itself. Want your wallpaper behind the text? Set `background_image` and pick how strongly it shows. Like a bit of motion? `trail = true` leaves a quick streak behind the cursor when it jumps.
- **Live config**: edit `ionterm.toml` and the change shows up while you're still in the editor.
- **Ligatures and Nerd Fonts**: `->`, `!=` and `===` join up, and icons render, including Powerline segments. The default font, JetBrainsMono Nerd Font, comes built into the Windows build, so it looks right there out of the box too.
- **Pixel-perfect box drawing**: lines, blocks, rounded corners and Powerline arrows are drawn by ionterm itself, so TUIs look sharp in any font.
- **Modern protocols**: kitty keyboard protocol, synchronized output, styled and coloured underlines, truecolor, bracketed paste, focus events, OSC 52 clipboard.
- **Mouse that behaves**: click-drag, double-click word and triple-click line selection, primary selection on middle click, and mouse reporting for TUIs (hold shift to select anyway).
- **Clickable links and files**: links that programs print and plain URLs get underlined on hover; ctrl+click opens them. Hold ctrl and file paths light up too: click `src/main.rs:42` in a compiler error and your editor opens right on that line.
- **Reflow**: resize the window and long lines re-wrap instead of getting chopped, which matters when Hyprland keeps retiling.
- **Tabs, Windows Terminal style**: rounded tabs up top with close and + buttons, drag to reorder, double-click to rename, right-click to colour or duplicate a tab, and an icon for whatever's running (fish, nvim, btop, ssh…). Prefer kitty's Powerline strip? It's one setting away.
- **Images in the terminal**: the kitty graphics protocol and Sixel, so `kitten icat`, chafa, yazi, ImageMagick and nvim image plugins show real pictures, even through tmux.
- **SSH, first class**: `ctrl+shift+s` lists every host in your `~/.ssh/config`. Connect in a tab or split, add or edit connections in a form (saved where plain `ssh` finds them too), give hosts a tab colour and icon, and get a "press Enter to reconnect" banner when a connection drops. ionterm sets up its terminfo on the server so you never see "unknown terminal".
- **Remote files**: `ctrl+shift+o` opens a file panel beside an SSH tab. Browse the server, follow your shell as you `cd`, download, upload, rename and delete, with progress bars and no second login. Or just drag files from your file manager onto the SSH tab and they land in the directory your shell is in. Transfers use rsync when the server has it (so a dropped connection resumes instead of starting over), and plain ssh when it doesn't.
- **Port forwards in a click**: "Forward a port…" (in an SSH tab's right-click menu or the command palette) lists what the server listens on; pick one and it's on `localhost`, through the connection you already have, so no second login. Forwards show as chips in the tab bar: click one to open it in your browser, right-click to stop it. Need the other direction, or a database behind the server? Type it in (`R 9000`, `5432:db.internal:5432`). Give a host its usual forwards in the connection editor and they come up every time you connect (and after a reconnect). Local dev servers get a chip too: start `npm run dev` and `:5173` appears, one click from your browser.
- **Edit server files on your computer**: type `ionterm-edit /etc/nginx/sites-available/mysite` on the server and it opens in your own editor in a new tab. Every save goes straight back to the server, keeping the file's owner and permissions, and if someone changed it there in the meantime ionterm asks before overwriting (or shows you the differences). Files only root can change? It offers sudo and asks for the password once. The command waits until you close the editor, so `export EDITOR=ionterm-edit` on the server makes `git commit` and `crontab -e` use your local editor too. In the file panel, Enter opens text files the same way.
- **Transfers from the shell**: type `ionterm-get report.pdf` on the server and it lands in your Downloads; `ionterm-put` asks which files to send. Old habits work too: `sz` and `rz` are handled right in the terminal, no lrzsz needed on your side.
- **Shell integration**: ctrl+shift+z and ctrl+shift+x hop between your prompts, ctrl+shift+g copies the last command's output, and ctrl+shift+right-click selects any command's output. Failed commands get a little red mark, hovering any old prompt tells you how that command ended, how long it took and when, and clicking in your command line moves the cursor there. Works in bash, zsh and fish, locally and over SSH, without touching your dotfiles.
- **Settings page**: the gear in the tab bar (or ctrl+,) opens a settings tab. Flip switches, pick a font from a list that shows each one in its own face, nudge sizes, and see it all apply instantly. Your `ionterm.toml` keeps its comments. Need the full file, your theme or your SSH config? One click opens it in your `$EDITOR` right there, and quitting the editor brings you back. Forgot a shortcut? The Shortcuts section lists every one, your own keybindings included, and finds them as you type.
- **Command palette**: ctrl+shift+p lists everything ionterm can do, with the keys for it, plus your SSH hosts. Type a few letters, hit Enter. You never have to remember a shortcut again (but you'll learn them anyway, they're right there).
- **Scrollback search**: ctrl+shift+f finds anything you've scrolled past, highlights every hit and hops between them with Enter. Lowercase searches ignore case, a capital makes it exact, and alt+r switches to regex. ctrl+shift+k wipes it when it gets cluttered. Or press ctrl+shift+h to read the whole scrollback, colours and all, in `less` (or any pager you like).
- **Hints**: press ctrl+shift+e and every URL, file path, `file:line`, git hash and IP on screen gets a little label. Type it to open the thing (files open in your editor at the right line), hold shift to copy it instead, or alt to type it into your command line. No mouse required.
- **Commands as blocks**: scroll back through a long build and its command stays pinned at the top (with ✓/✗, how long it took and when), click it to jump to the prompt. A thin scrollbar shows up while you scroll, with marks for every prompt, failed command and search hit. Hover a command and three little dots appear in the margin: copy the command, its output or both as Markdown, run it again, open the output in your editor or pager, save it, or fold 5,000 lines of output down to six. Nothing gets in the way of selecting text, and full-screen apps never see any of it. The header and the scrollbar can each be switched off in settings.
- **Safe pasting**: pasting several lines into something that would run each one immediately (a bare `sh`, a `sudo` prompt, an old bash) asks first, and offers to join them into one line instead. Shells that handle pastes safely are never bothered.
- **Never miss a finished build**: tabs you're not looking at show a dot for new output, a bell when something rings, and a ✓ or ✗ when a long command finishes. If you're in another window, you get a desktop notification too ("✗ cargo build (exit 101), finished after 2m 03s"). Programs can send their own notifications with OSC 9 / 777.
- **Progress in the tab**: downloads, builds and updates fill a bar along the bottom of their tab, whether the program reports its progress or just prints `45%`.
- **One ionterm, many windows**: ctrl+shift+n (or launching ionterm again) opens a window in the ionterm that's already running, which is quicker and lets tabs move around. Drag a tab out of the tab bar to give it its own window, or drop it on another window's tab bar. Right-click a tab for "Move to new window" and "Move to window…", or move a single pane into its own tab, its own window or another tab as a split from the command palette. Shells, scrollback, SSH connections and running transfers all come along. Want a separate process anyway? `ionterm --standalone`.
- **Splits and layouts**: split any pane and drag the borders between them, type into all of them at once with ctrl+shift+b (handy for a few servers), or let kitty-style layouts (tall, fat, grid, stack) arrange them. New tabs and splits open in the directory you're in.
- **Pick up where you left off**: turn on `restore` under `[session]` and next time every window reopens with its tabs, splits, folders and SSH connections, exactly as you had them. Close every tab yourself and you start fresh.
- **Scriptable**: `ionterm @` drives the terminal from scripts: open windows, tabs and splits running commands, move tabs and panes between windows, type into panes, read their text, retitle tabs or run any keybinding action. See [Remote control](#remote-control).
- **SSH-friendly**: `TERM` falls back to `xterm-256color` whenever the remote end might not know `xterm-ionterm`, so no more "unknown terminal".

Coming up: more. See [the plan](docs/PLAN.md).

## Configuration

Config lives in `~/.config/ionterm/` (`%APPDATA%\ionterm\` on Windows):

- `ionterm.toml`: fonts, window, cursor, scrollback and keybindings. Every key is optional; [extras/ionterm.toml](extras/ionterm.toml) lists them all with their defaults.
- `theme.toml`: colours. The Ionix styler links this file in for you; without one you get the Ionix palette on a soft dark background.

Run `ionterm -e <command>` to start something other than your shell, for example `ionterm -e btop`, and `ionterm -d <directory>` to start in a specific folder (`ionterm -d ~/projects -e nvim` works too). With ionterm already running, new windows open in it (`single_instance` under `[terminal]`); `--standalone` starts a separate ionterm instead. Only the same program is joined, so a development build never hands its windows to the installed one.

## Remote control

Anything running inside ionterm can control it with `ionterm @ <command>` (it finds its own window and pane through `IONTERM_SOCKET` and `IONTERM_PANE`):

```sh
ionterm @ ls                                  # windows, tabs and panes, tab-separated
ionterm @ split --right -- htop               # prints the new pane's id
ionterm @ new-tab --title Logs --cwd /var/log journalctl -f
ionterm @ new-window --host ionserver          # a window connected to a host
ionterm @ move-tab --new-window               # this tab gets a window of its own
ionterm @ move-pane --tab 1 --window 2        # this pane joins window 2's first tab
ionterm @ forward 3000                         # the server's port 3000 on localhost; prints the local port
ionterm @ send-text -p 3 'cargo test\n'        # type into pane 3
ionterm @ get-text -p 3 -a > build.log        # its whole scrollback
ionterm @ set-title "deploy"                  # this tab's name
ionterm @ action toggle_zoom                  # any keybinding action
```

`ionterm @ help` lists everything. The socket lives in `$XDG_RUNTIME_DIR` and only your user can open it; set `remote_control = false` under `[terminal]` to turn it off.

## Technical details

- Rust workspace: `ionterm-vt` (terminal state, no I/O), `ionterm-render` (wgpu renderer and fonts), `ionterm` (the app).
- Build with `cargo build --release`; the binary ends up in `target/release/ionterm`. On Arch, `scripts/install-pkg.sh` builds and installs the `ionterm-git` package (it runs `makepkg -si` in a temporary folder under `~/.cache`, so nothing is left in the working directory; other makepkg options can be passed instead). The package builds what's pushed to GitHub, so the script asks first when there are unpushed commits or uncommitted changes.
- To use the dev build as a regular app (launcher entry and taskbar icon), run `scripts/install-dev.sh`; `scripts/install-dev.sh uninstall` removes it again.
- Run the tests with `cargo test --workspace`. Set `IONTERM_LOG=debug` to see unhandled escape sequences, or `IONTERM_DUMP=<file>` to record everything programs print.
- The styler files to upstream into Ionix are in [extras/ionix-styler](extras/ionix-styler).
