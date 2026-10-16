//! The command ionterm runs on the remote side of a connection it opens.
//!
//! It makes sure the remote end knows ionterm's `TERM` (installing the
//! terminfo entry with `tic` when it can, otherwise falling back to
//! `xterm-256color`). The entry usually lands in the user's own
//! `~/.terminfo`, which the next host and other users can't see, so `ssh`,
//! `sudo`, `su` and `doas` get `xterm-256color`. It then starts the user's login shell with a small hook
//! that reports the working directory (OSC 7), the shell integration that
//! marks prompts and commands (OSC 133, `crate::shell`), and the
//! `ionterm-get`, `ionterm-put` and `ionterm-edit` commands (OSC 5379), for this session only.
//! Hook files live in a temporary directory that the shell deletes once read; `ionterm-edit`
//! is a script (so it works as `$EDITOR`) in another one on `PATH`, removed when the shell exits.
//!
//! The script is sent base64-encoded inside a single-quoted `sh -c`, which
//! survives being parsed by bash, zsh, fish or csh as the login shell.

use crate::shell;

const TERMINFO_SOURCE: &str = include_str!("../../../../extras/terminfo/ionterm.terminfo");

/// The POSIX shell script; `@TERMINFO@` is replaced by the base64 terminfo source.
const SCRIPT: &str = r#"
TI='@TERMINFO@'
if command -v infocmp >/dev/null 2>&1 && ! infocmp xterm-ionterm >/dev/null 2>&1 && command -v tic >/dev/null 2>&1; then
  printf %s "$TI" | base64 -d 2>/dev/null | tic -x - >/dev/null 2>&1
fi
if command -v infocmp >/dev/null 2>&1 && infocmp xterm-ionterm >/dev/null 2>&1; then
  TERM=xterm-ionterm
else
  TERM=xterm-256color
fi
COLORTERM=truecolor
TERM_PROGRAM=ionterm
export TERM COLORTERM TERM_PROGRAM
host=$(hostname 2>/dev/null || uname -n)
shell=${SHELL:-/bin/sh}
tmp=$(mktemp -d 2>/dev/null || mktemp -d -t ionterm)
bin=$(mktemp -d 2>/dev/null || mktemp -d -t ionterm-bin)
cat > "$bin/ionterm-edit" <<'IONTERM_EOF'
#!/bin/sh
# ionterm-edit <file>: edit a file of this server in the editor of the computer
# running ionterm, and wait until it's closed. Works as $EDITOR too.
if [ $# -ne 1 ]; then echo 'Usage: ionterm-edit <file>' >&2; exit 2; fi
# Not talking to ionterm (tmux, another terminal): an ordinary editor.
if [ "$TERM_PROGRAM" != ionterm ] || ! [ -r /dev/tty ]; then exec "${IONTERM_EDIT_FALLBACK:-vi}" "$1"; fi
case $1 in /*) p=$1 ;; *) p=$PWD/$1 ;; esac
if [ -d "$p" ]; then echo "ionterm-edit: $1: Is a directory" >&2; exit 1; fi
n=$(od -An -N8 -tx1 /dev/urandom 2>/dev/null | tr -d ' \n'); [ -n "$n" ] || n=x$$
s=$(stty -g </dev/tty 2>/dev/null)
trap 'stty "$s" </dev/tty 2>/dev/null; exit 130' INT TERM HUP
stty -echo </dev/tty 2>/dev/null
printf '\033]5379;edit;%s;%s\007' "$(printf %s "$p" | base64 | tr -d '\n')" "$n" >/dev/tty
printf 'Editing %s in ionterm; close the editor to go on (ctrl+c stops waiting).\n' "$1" >/dev/tty
r=
while IFS= read -r line </dev/tty; do
  case $line in *"$n "*) r=${line#*"$n "}; break ;; esac
done
stty "$s" </dev/tty 2>/dev/null
case $r in
  saved|unchanged) exit 0 ;;
  *) echo "ionterm-edit: ${r:-ionterm didn't answer}" >&2; exit 1 ;;
esac
IONTERM_EOF
chmod 700 "$bin/ionterm-edit"
cat > "$tmp/fns" <<'IONTERM_EOF'
__ionterm_b64() { printf %s "$1" | base64 | tr -d '\n'; }
ionterm-get() {
  if [ $# -eq 0 ]; then echo 'Usage: ionterm-get <file>...' >&2; return 2; fi
  local f p k
  for f in "$@"; do
    case $f in /*) p=$f ;; *) p=$PWD/$f ;; esac
    if [ ! -e "$p" ]; then echo "ionterm-get: $f: No such file or directory" >&2; continue; fi
    k=get; [ -d "$p" ] && k=get-dir
    printf '\033]5379;%s;%s\007' "$k" "$(__ionterm_b64 "$p")"
  done
}
ionterm-put() {
  local d=${1:-$PWD}
  case $d in /*) ;; *) d=$PWD/$d ;; esac
  if [ ! -d "$d" ]; then echo "ionterm-put: $d: Not a directory" >&2; return 1; fi
  printf '\033]5379;put;%s\007' "$(__ionterm_b64 "$d")"
}
__ionterm_term() {
  if [ "$TERM" = xterm-ionterm ]; then TERM=xterm-256color command "$@"; else command "$@"; fi
}
typeset -f ssh >/dev/null 2>&1 || function ssh { __ionterm_term ssh "$@"; }
typeset -f sudo >/dev/null 2>&1 || function sudo { __ionterm_term sudo "$@"; }
typeset -f su >/dev/null 2>&1 || function su { __ionterm_term su "$@"; }
typeset -f doas >/dev/null 2>&1 || function doas { __ionterm_term doas "$@"; }
IONTERM_EOF
printf "__ionterm_bin='%s'\n" "$bin" >> "$tmp/fns"
cat >> "$tmp/fns" <<'IONTERM_EOF'
__ionterm_path() { case ":$PATH:" in *":$__ionterm_bin:"*) ;; *) PATH=$__ionterm_bin:$PATH; export PATH ;; esac; }
__ionterm_path
__ionterm_rmbin() { rm -rf -- "$__ionterm_bin"; }
if [ -n "$ZSH_VERSION" ]; then
  # .zprofile and .zshrc run after this and may set PATH outright.
  precmd_functions+=(__ionterm_path)
  zshexit_functions+=(__ionterm_rmbin)
elif [ -z "$(trap -p EXIT 2>/dev/null)" ]; then
  trap __ionterm_rmbin EXIT
fi
IONTERM_EOF
case "${shell##*/}" in
  fish)
    cat > "$tmp/ionterm.fish" <<'IONTERM_EOF'
set -q hostname; or set -g hostname (uname -n)
function __ionterm_osc7 --on-event fish_prompt
    printf '\e]7;file://%s%s\a' $hostname (string escape --style=url -- $PWD)
end
# fish 4 marks prompts and commands itself.
if test (string split -f1 . $version) -lt 4
@FISH_INTEGRATION@
end
function __ionterm_b64
    printf %s $argv[1] | base64 | string join ''
end
function ionterm-get --description 'Download files to this computer, through ionterm'
    if test (count $argv) -eq 0
        echo 'Usage: ionterm-get <file>...' >&2
        return 2
    end
    for f in $argv
        set -l p $f
        string match -q -- '/*' $p; or set p $PWD/$f
        if not test -e $p
            echo "ionterm-get: $f: No such file or directory" >&2
            continue
        end
        set -l k get
        test -d $p; and set k get-dir
        printf '\e]5379;%s;%s\a' $k (__ionterm_b64 $p)
    end
end
function ionterm-put --description 'Upload files from this computer, through ionterm'
    set -l d $PWD
    test (count $argv) -gt 0; and set d $argv[1]
    string match -q -- '/*' $d; or set d $PWD/$d
    if not test -d $d
        echo "ionterm-put: $d: Not a directory" >&2
        return 1
    end
    printf '\e]5379;put;%s\a' (__ionterm_b64 $d)
end
for c in ssh sudo su doas
    functions -q $c; and continue
    function $c --inherit-variable c
        if test "$TERM" = xterm-ionterm
            env TERM=xterm-256color $c $argv
        else
            command $c $argv
        end
    end
end
__ionterm_osc7
IONTERM_EOF
    printf "set -g __ionterm_bin '%s'\n" "$bin" >> "$tmp/ionterm.fish"
    cat >> "$tmp/ionterm.fish" <<'IONTERM_EOF'
contains -- $__ionterm_bin $PATH; or set -gx PATH $__ionterm_bin $PATH
function __ionterm_rmbin --on-event fish_exit
    rm -rf -- $__ionterm_bin
end
IONTERM_EOF
    exec "$shell" -l -C "source '$tmp/ionterm.fish'; rm -rf '$tmp'"
    ;;
  bash)
    cat > "$tmp/rc" <<'IONTERM_EOF'
[ -f /etc/profile ] && . /etc/profile
if [ -f ~/.bash_profile ]; then . ~/.bash_profile
elif [ -f ~/.bash_login ]; then . ~/.bash_login
elif [ -f ~/.profile ]; then . ~/.profile
fi
@BASH_INTEGRATION@
IONTERM_EOF
    cat "$tmp/fns" >> "$tmp/rc" 2>/dev/null
    printf '%s\n' 'rm -rf "$(dirname "${BASH_SOURCE[0]}")"' >> "$tmp/rc"
    exec "$shell" --rcfile "$tmp/rc" -i
    ;;
  zsh)
    cat > "$tmp/.zshenv" <<'IONTERM_EOF'
tmp_dir=$ZDOTDIR
ZDOTDIR=$HOME
[ -f "$HOME/.zshenv" ] && . "$HOME/.zshenv"
@ZSH_INTEGRATION@
IONTERM_EOF
    cat "$tmp/fns" >> "$tmp/.zshenv" 2>/dev/null
    printf '%s\n' 'rm -rf "$tmp_dir"' 'unset tmp_dir' >> "$tmp/.zshenv"
    ZDOTDIR=$tmp exec "$shell" -l
    ;;
  *)
    # No hooks for this shell, so nothing would keep TERM from leaking.
    TERM=xterm-256color
    PATH=$bin:$PATH
    export PATH
    rm -rf "$tmp"
    exec "$shell" -l
    ;;
esac
"#;

/// The remote command for `ssh -t <host> <command>`.
pub fn remote_command() -> String {
    let script = SCRIPT
        .replace("@TERMINFO@", &base64(TERMINFO_SOURCE.as_bytes()))
        .replace("@BASH_INTEGRATION@", shell::BASH)
        .replace("@ZSH_INTEGRATION@", shell::ZSH)
        .replace("@FISH_INTEGRATION@", shell::FISH);
    format!("exec sh -c 'eval \"$(printf %s {} | base64 -d)\"'", base64(script.as_bytes()))
}

pub(crate) fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - i * 6)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_has_only_safe_characters() {
        let cmd = remote_command();
        // Everything inside the single quotes must be free of quotes and
        // backslashes, so every login shell passes it through untouched.
        let inner = cmd.strip_prefix("exec sh -c '").and_then(|s| s.strip_suffix('\'')).unwrap();
        assert!(!inner.contains('\'') && !inner.contains('\\'));
    }

    #[test]
    fn script_runs_in_sh() {
        // The script must at least parse in a POSIX shell.
        let script = SCRIPT.replace("@TERMINFO@", "");
        let status = std::process::Command::new("sh").args(["-n", "-c", &script]).status();
        if let Ok(status) = status {
            assert!(status.success());
        }
    }
}
