# Ionix styler integration

Files to upstream into the Ionix repo (`profile/airootfs/usr/local/share/ionix/styler/`) once ionterm becomes the default terminal:

- `themes/<theme>/ionterm/theme.toml`: the static colours for each theme.
- `themes/ionix-adaptive/adaptive/ionterm/theme.toml`: the adaptive template, with `{{slot}}` tokens rendered by `ionix-adaptive-colors`.
- `install-ionterm.sh`: the `ionix_install_ionterm` / `ionix_uninstall_ionterm` functions for `install-default`.

The themes were generated from each theme's `kitty/kitty.conf` with `kitty-to-ionterm.sh`. To regenerate one, or to convert a theme added later, run:

```bash
./kitty-to-ionterm.sh <theme>/kitty/kitty.conf themes/<theme>/ionterm/theme.toml
```

ionterm watches `~/.config/ionterm/` and reloads within about 250 ms when the styler re-links `theme.toml`, so no reload signal is needed.
