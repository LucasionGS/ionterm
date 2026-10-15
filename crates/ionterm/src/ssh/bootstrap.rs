//! The command ionterm runs on the remote side of a connection it opens.
//!
//! It makes sure the remote end knows ionterm's `TERM` (installing the
//! terminfo entry with `tic` when it can, otherwise falling back to
//! `xterm-256color`), then starts the user's login shell with a small hook
//! that reports the working directory (OSC 7), and the `ionterm-get` and
//! `ionterm-put` commands (OSC 5379), for this session only. Hook files live
//! in a temporary directory that the shell deletes once read.
//!
//! The script is sent base64-encoded inside a single-quoted `sh -c`, which
//! survives being parsed by bash, zsh, fish or csh as the login shell.

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
IONTERM_EOF
case "${shell##*/}" in
  fish)
    cat > "$tmp/ionterm.fish" <<'IONTERM_EOF'
set -q hostname; or set -g hostname (uname -n)
function __ionterm_osc7 --on-event fish_prompt
    printf '\e]7;file://%s%s\a' $hostname (string escape --style=url -- $PWD)
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
__ionterm_osc7
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
__ionterm_osc7() { printf '\e]7;file://%s%s\a' "$HOSTNAME" "$PWD"; }
PROMPT_COMMAND="__ionterm_osc7${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
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
__ionterm_osc7() { printf '\e]7;file://%s%s\a' "$HOST" "$PWD"; }
autoload -Uz add-zsh-hook && add-zsh-hook precmd __ionterm_osc7
IONTERM_EOF
    cat "$tmp/fns" >> "$tmp/.zshenv" 2>/dev/null
    printf '%s\n' 'rm -rf "$tmp_dir"' 'unset tmp_dir' >> "$tmp/.zshenv"
    ZDOTDIR=$tmp exec "$shell" -l
    ;;
  *)
    rm -rf "$tmp"
    exec "$shell" -l
    ;;
esac
"#;

/// The remote command for `ssh -t <host> <command>`.
pub fn remote_command() -> String {
    let script = SCRIPT.replace("@TERMINFO@", &base64(TERMINFO_SOURCE.as_bytes()));
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
