//! Recognising an `ssh` the user typed in a shell, from its command line.

/// Options of `ssh` that take a value, so the value isn't mistaken for the host.
pub const OPTIONS_WITH_VALUES: &str = "BbcDEeFIiJLlmOoPpQRSWw";

/// `ssh` arguments given to ionterm (`ionterm ssh -J jump user@host -p 2222`),
/// split the way `ssh` reads them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SshArgs {
    /// The options, one flag per word (`-tp22` becomes `-t -p 22`), so single
    /// flags can be picked out.
    pub options: Vec<String>,
    pub destination: String,
    /// A remote command, run instead of a login shell.
    pub command: Vec<String>,
}

/// Parses `ssh` arguments (without the program). Like `ssh`, options may
/// come before or after the destination; the first word after the
/// destination that isn't an option starts the remote command.
pub fn parse_args(args: &[String]) -> Result<SshArgs, String> {
    let mut parsed = SshArgs::default();
    let mut destination = None;
    let mut ended = false; // after `--`, nothing more is an option
    let mut words = args.iter().enumerate();
    while let Some((index, word)) = words.next() {
        if !ended && word == "--" {
            ended = true;
            continue;
        }
        match word.strip_prefix('-').filter(|flags| !ended && !flags.is_empty()) {
            Some(flags) => {
                for (i, c) in flags.char_indices() {
                    parsed.options.push(format!("-{c}"));
                    if OPTIONS_WITH_VALUES.contains(c) {
                        let rest = &flags[i + c.len_utf8()..];
                        let value = if rest.is_empty() { words.next().map(|(_, v)| v.as_str()) } else { Some(rest) };
                        let Some(value) = value else {
                            return Err(format!("Missing value for -{c}."));
                        };
                        parsed.options.push(value.to_string());
                        break;
                    }
                }
            }
            None if destination.is_none() => destination = Some(word.clone()),
            None => {
                parsed.command = args[index..].to_vec();
                break;
            }
        }
    }
    parsed.destination = destination.ok_or("Missing destination.")?;
    Ok(parsed)
}

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

    #[test]
    fn parses_given_args() {
        let a = parse_args(&argv("-J jump@10.0.0.1 me@box.example.com -p 11996")).unwrap();
        assert_eq!(a.options, argv("-J jump@10.0.0.1 -p 11996"));
        assert_eq!(a.destination, "me@box.example.com");
        assert!(a.command.is_empty());

        let a = parse_args(&argv("-tAp22 box htop -d 5")).unwrap();
        assert_eq!(a.options, argv("-t -A -p 22"));
        assert_eq!(a.command, argv("htop -d 5"));

        let a = parse_args(&argv("-- box -p 22")).unwrap();
        assert!(a.options.is_empty());
        assert_eq!(a.command, argv("-p 22"));

        assert!(parse_args(&argv("-p")).is_err());
        assert!(parse_args(&argv("-v -4")).is_err());
    }
}
