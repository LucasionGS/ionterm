# ionterm

A fast, GPU-rendered terminal for the Ionix desktop. It picks up your wallpaper colours, draws its box lines without gaps, and has room for kitty-style tricks.

![ionterm showing box drawing, Powerline segments, ligatures and emoji](docs/screenshot.png)
<sup>The Ionix theme, mid-demo. Yes, the box lines actually touch.</sup>

## Key Features

- **GPU rendering**: everything is drawn with wgpu (Vulkan on Linux, DX12 on Windows), so scrolling through a huge log doesn't heat your lap.
- **Ionix theming**: plugs into the Ionix styler like kitty and ioexplorer do, wallpaper-adaptive colours included. Switch themes and the open terminal recolours itself.
- **Live config**: edit `ionterm.toml` and the change shows up while you're still in the editor.
- **Ligatures and Nerd Fonts**: `->`, `!=` and `===` join up, and icons render, including Powerline segments.
- **Pixel-perfect box drawing**: lines, blocks, rounded corners and Powerline arrows are drawn by ionterm itself, so TUIs look sharp in any font.
- **Modern protocols**: kitty keyboard protocol, synchronized output, styled and coloured underlines, truecolor, bracketed paste, focus events, OSC 52 clipboard.
- **Mouse that behaves**: click-drag, double-click word and triple-click line selection, primary selection on middle click, and mouse reporting for TUIs (hold shift to select anyway).
- **Clickable links**: links that programs print and plain URLs get underlined on hover; ctrl+click opens them.
- **Reflow**: resize the window and long lines re-wrap instead of getting chopped, which matters when Hyprland keeps retiling.
- **Tabs, Windows Terminal style**: rounded tabs up top with close and + buttons, drag to reorder, double-click to rename, right-click to colour or duplicate a tab, and an icon for whatever's running (fish, nvim, btop, ssh…). Prefer kitty's Powerline strip? It's one setting away.
- **Images in the terminal**: the kitty graphics protocol and Sixel, so `kitten icat`, chafa, yazi, ImageMagick and nvim image plugins show real pictures, even through tmux.
- **SSH, first class**: `ctrl+shift+s` lists every host in your `~/.ssh/config`. Connect in a tab or split, add or edit connections in a form (saved where plain `ssh` finds them too), give hosts a tab colour and icon, and get a "press Enter to reconnect" banner when a connection drops. ionterm sets up its terminfo on the server so you never see "unknown terminal".
- **Remote files**: `ctrl+shift+o` opens a file panel beside an SSH tab. Browse the server, follow your shell as you `cd`, download, upload, rename and delete, with progress bars and no second login. Or just drag files from your file manager onto the SSH tab and they land in the directory your shell is in. Transfers use rsync when the server has it (so a dropped connection resumes instead of starting over), and plain ssh when it doesn't.
- **Transfers from the shell**: type `ionterm-get report.pdf` on the server and it lands in your Downloads; `ionterm-put` asks which files to send. Old habits work too: `sz` and `rz` are handled right in the terminal, no lrzsz needed on your side.
- **Shell integration**: ctrl+shift+z and ctrl+shift+x hop between your prompts, ctrl+shift+g copies the last command's output, and ctrl+shift+right-click selects any command's output. Failed commands get a little red mark, and clicking in your command line moves the cursor there. Works in bash, zsh and fish, locally and over SSH, without touching your dotfiles.
- **Settings page**: the gear in the tab bar (or ctrl+,) opens a settings tab. Flip switches, pick a font from a list that shows each one in its own face, nudge sizes, and see it all apply instantly. Your `ionterm.toml` keeps its comments. Need the full file, your theme or your SSH config? One click opens it in your `$EDITOR` right there, and quitting the editor brings you back.
- **Command palette**: ctrl+shift+p lists everything ionterm can do, with the keys for it, plus your SSH hosts. Type a few letters, hit Enter. You never have to remember a shortcut again (but you'll learn them anyway, they're right there).
- **Scrollback search**: ctrl+shift+f finds anything you've scrolled past, highlights every hit and hops between them with Enter. Lowercase searches ignore case, a capital makes it exact, and alt+r switches to regex.
- **Hints**: press ctrl+shift+e and every URL, file path, `file:line`, git hash and IP on screen gets a little label. Type it to open the thing (files open in your editor at the right line), hold shift to copy it instead, or alt to type it into your command line. No mouse required.
- **Never miss a finished build**: tabs you're not looking at show a dot for new output, a bell when something rings, and a ✓ or ✗ when a long command finishes. If you're in another window, you get a desktop notification too ("✗ cargo build (exit 101), finished after 2m 03s"). Programs can send their own notifications with OSC 9 / 777.
- **Progress in the tab**: downloads, builds and updates fill a bar along the bottom of their tab, whether the program reports its progress or just prints `45%`.
- **Splits and layouts**: split any pane, or let kitty-style layouts (tall, fat, grid, stack) arrange them. New tabs and splits open in the directory you're in.
- **SSH-friendly**: `TERM` falls back to `xterm-256color` whenever the remote end might not know `xterm-ionterm`, so no more "unknown terminal".

Coming up: more. See [the plan](docs/PLAN.md). See [the plan](docs/PLAN.md).

## Configuration

Config lives in `~/.config/ionterm/` (`%APPDATA%\ionterm\` on Windows):

- `ionterm.toml`: fonts, window, cursor, scrollback and keybindings. Every key is optional; [extras/ionterm.toml](extras/ionterm.toml) lists them all with their defaults.
- `theme.toml`: colours. The Ionix styler links this file in for you; without one you get the Ionix palette on a soft dark background.

Run `ionterm -e <command>` to start something other than your shell, for example `ionterm -e btop`.

## Technical details

- Rust workspace: `ionterm-vt` (terminal state, no I/O), `ionterm-render` (wgpu renderer and fonts), `ionterm` (the app).
- Build with `cargo build --release`; the binary ends up in `target/release/ionterm`. On Arch, `makepkg -si` with the included PKGBUILD.
- To use the dev build as a regular app (launcher entry and taskbar icon), run `scripts/install-dev.sh`; `scripts/install-dev.sh uninstall` removes it again.
- Run the tests with `cargo test --workspace`. Set `IONTERM_LOG=debug` to see unhandled escape sequences, or `IONTERM_DUMP=<file>` to record everything programs print.
- The styler files to upstream into Ionix are in [extras/ionix-styler](extras/ionix-styler).
