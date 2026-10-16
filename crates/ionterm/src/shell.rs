//! Shell integration for the local login shell: bash, zsh and fish 3 are
//! started with a small script that marks prompts and commands (OSC 133),
//! without touching the user's own startup files. fish 4 sends the marks
//! itself. The same scripts go into the SSH bootstrap (`ssh/bootstrap.rs`).
//!
//! - bash: `--rcfile` with a file that loads the login files the way a
//!   login shell would, then the integration.
//! - zsh: `ZDOTDIR` points at a folder whose `.zshenv` puts `ZDOTDIR` back,
//!   loads the user's `.zshenv`, then the integration.
//! - fish 3: `-C` sources the integration after the user's config.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const BASH: &str = include_str!("../../../extras/shell-integration/ionterm.bash");
pub const ZSH: &str = include_str!("../../../extras/shell-integration/ionterm.zsh");
pub const FISH: &str = include_str!("../../../extras/shell-integration/ionterm.fish");

/// How to start a shell with the integration.
#[derive(Debug, PartialEq, Eq)]
pub enum Setup {
    /// Run this command line instead.
    Command(Vec<String>),
    /// Run the shell as before, with these variables set.
    Env(Vec<(String, String)>),
}

/// How to start `shell` (a path or name) with the integration, if it's
/// one ionterm knows. Writes the scripts on first use.
pub fn setup(shell: &str) -> Option<Setup> {
    if cfg!(windows) {
        return None;
    }
    let dir = scripts_dir()?;
    let name = Path::new(shell).file_name()?.to_str()?;
    match name {
        "bash" => Some(Setup::Command(vec![
            shell.into(),
            "--rcfile".into(),
            dir.join("bash/ionterm.bashrc").to_string_lossy().into_owned(),
            "-i".into(),
        ])),
        "zsh" => {
            let mut vars = vec![("ZDOTDIR".into(), dir.join("zsh").to_string_lossy().into_owned())];
            // Where the user's own files are, to put it back.
            if let Ok(original) = std::env::var("ZDOTDIR") {
                vars.push(("IONTERM_ZDOTDIR".into(), original));
            }
            Some(Setup::Env(vars))
        }
        "fish" if fish_major(shell).is_some_and(|major| major < 4) => Some(Setup::Command(vec![
            shell.into(),
            "-l".into(),
            "-C".into(),
            format!("source '{}'", dir.join("fish/ionterm.fish").display()),
        ])),
        _ => None,
    }
}

/// The scripts, written once per run into a private folder.
fn scripts_dir() -> Option<PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = dirs::runtime_dir().or_else(dirs::cache_dir)?.join("ionterm").join("shell");
        let write = |path: &str, text: &str| -> std::io::Result<()> {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().expect("has a folder"))?;
            std::fs::write(path, text)
        };
        let result = write("bash/ionterm.bashrc", &bash_rc())
            .and_then(|()| write("zsh/.zshenv", &zsh_env()))
            .and_then(|()| write("fish/ionterm.fish", FISH));
        match result {
            Ok(()) => Some(dir),
            Err(error) => {
                tracing::warn!("shell integration unavailable: {error}");
                None
            }
        }
    })
    .clone()
}

/// What a login bash reads, then the integration.
fn bash_rc() -> String {
    format!(
        r#"# Written by ionterm: loads your login files, then its shell integration.
[ -f /etc/profile ] && . /etc/profile
if [ -f ~/.bash_profile ]; then . ~/.bash_profile
elif [ -f ~/.bash_login ]; then . ~/.bash_login
elif [ -f ~/.profile ]; then . ~/.profile
fi
{BASH}"#
    )
}

/// Puts `ZDOTDIR` back so zsh finds the user's files, loads their
/// `.zshenv`, then the integration.
fn zsh_env() -> String {
    format!(
        r#"# Written by ionterm: restores ZDOTDIR, loads your .zshenv, then its shell integration.
if [[ -n $IONTERM_ZDOTDIR ]]; then
  ZDOTDIR=$IONTERM_ZDOTDIR
  unset IONTERM_ZDOTDIR
else
  unset ZDOTDIR
fi
[[ -f ${{ZDOTDIR:-$HOME}}/.zshenv ]] && . "${{ZDOTDIR:-$HOME}}/.zshenv"
{ZSH}"#
    )
}

/// fish's major version, asked once.
fn fish_major(fish: &str) -> Option<u32> {
    static MAJOR: OnceLock<Option<u32>> = OnceLock::new();
    *MAJOR.get_or_init(|| {
        // Example: fish, version 3.7.1
        let output = std::process::Command::new(fish).arg("--version").output().ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        text.rsplit(' ').next()?.split('.').next()?.trim().parse().ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_the_shells() {
        // No integration on Windows: its shells are started as they are.
        if cfg!(windows) {
            assert_eq!(setup("bash"), None);
            return;
        }
        assert!(matches!(setup("/usr/bin/bash"), Some(Setup::Command(argv)) if argv[1] == "--rcfile"));
        assert!(matches!(setup("zsh"), Some(Setup::Env(vars)) if vars[0].0 == "ZDOTDIR"));
        assert_eq!(setup("/usr/bin/nu"), None);
    }
}
