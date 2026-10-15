//! Recognising an `ssh` the user typed in a shell, from its command line.

/// Options of `ssh` that take a value, so the value isn't mistaken for the host.
const OPTIONS_WITH_VALUES: &str = "BbcDEeFIiJLlmOoPpQRSWw";

/// A typed ssh session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetectedSsh {
    /// The destination as typed (`host`, `user@host`, `ssh://…`).
    pub destination: String,
    /// The full command, to run again for a duplicate tab.
    pub argv: Vec<String>,
}

impl DetectedSsh {
    /// The host part of the destination, for display.
    pub fn host(&self) -> &str {
        let d = self.destination.strip_prefix("ssh://").unwrap_or(&self.destination);
        let d = d.rsplit_once('@').map_or(d, |(_, host)| host);
        d.split(':').next().unwrap_or(d)
    }
}

/// Parses an `ssh` command line. Returns `None` for anything that isn't an
/// interactive connection (no destination).
pub fn parse_ssh_argv(argv: &[String]) -> Option<DetectedSsh> {
    let program = argv.first()?;
    let name = std::path::Path::new(program).file_name()?.to_string_lossy();
    if name != "ssh" {
        return None;
    }
    let mut args = argv[1..].iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            return args.next().map(|d| detected(d, argv));
        }
        if let Some(flags) = arg.strip_prefix('-') {
            // Combined flags (`-tt`, `-p22`): a value-taking flag consumes
            // the rest of the word, or the next argument if nothing is left.
            for (i, c) in flags.char_indices() {
                if OPTIONS_WITH_VALUES.contains(c) {
                    if i + c.len_utf8() == flags.len() {
                        args.next();
                    }
                    break;
                }
            }
            continue;
        }
        return Some(detected(arg, argv));
    }
    None
}

fn detected(destination: &str, argv: &[String]) -> DetectedSsh {
    DetectedSsh {
        destination: destination.to_string(),
        argv: argv.to_vec(),
    }
}

/// Reads a process's command line (Linux).
pub fn process_argv(pid: i64) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        raw.split(|&b| b == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn finds_the_destination() {
        let d = parse_ssh_argv(&argv("ssh -p 2222 -i ~/.ssh/key deploy@web.example.com uptime")).unwrap();
        assert_eq!(d.destination, "deploy@web.example.com");
        assert_eq!(d.host(), "web.example.com");
        assert_eq!(parse_ssh_argv(&argv("/usr/bin/ssh -tt -p22 hostion")).unwrap().host(), "hostion");
        assert_eq!(parse_ssh_argv(&argv("ssh -o ControlMaster=no -- box")).unwrap().destination, "box");
        assert!(parse_ssh_argv(&argv("ssh -V")).is_none());
        assert!(parse_ssh_argv(&argv("sshd -D")).is_none());
    }
}
