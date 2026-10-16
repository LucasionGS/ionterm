//! ionterm: a GPU-rendered terminal emulator for the Ionix desktop.

mod app;
mod commands;
mod config;
mod dialog;
mod dnd;
mod editor;
mod hints;
mod files;
mod input;
mod layout;
mod links;
mod menu;
mod notifications;
mod palette;
mod pane;
mod pty;
mod remote;
mod scrollback;
mod search;
mod session;
mod settings;
mod shell;
mod ssh;
mod tab;
mod tabbar;
mod theme;
mod zmodem;

use std::process::ExitCode;

use tracing_subscriber::EnvFilter;
use winit::event_loop::EventLoop;

use crate::app::{App, UserEvent};
use crate::config::Config;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_help() {
    println!("ionterm {VERSION}: a GPU-rendered terminal emulator\n");
    println!("Usage: ionterm [options] [-e <command> [args...]]");
    println!("       ionterm host <host>      Open a tab connected to an ssh config host");
    println!("       ionterm ssh <args...>    Open a tab connected with ssh arguments (\"ionterm ssh -p 2222 user@host\")");
    println!("       ionterm @ <command>      Control a running ionterm (\"ionterm @ help\" lists commands)\n");
    println!("Options:");
    println!("  -e, --exec <command>   Run <command> instead of the shell (takes the rest of the line)");
    println!("  -w, --new-window       With host or ssh: open a new window instead of a tab in the running ionterm");
    println!("  -h, --help             Show this help");
    println!("  -v, --version          Show the version");
    println!("\nConfig: {}", config::config_dir().map_or("(none)".into(), |d| d.join(config::CONFIG_FILE).display().to_string()));
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("IONTERM_LOG").unwrap_or_else(|_| EnvFilter::new("ionterm=info,warn")),
        )
        .init();

    // `ionterm @ …` only talks to a running window.
    if std::env::args().nth(1).as_deref() == Some("@") {
        return remote::client(std::env::args().skip(2).collect());
    }

    let mut config = Config::load();
    let mut new_window = false;
    // `host` and `ssh` go to a running ionterm as a new tab, unless asked not to.
    let mut forward = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" | "help" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            "-v" | "--version" | "version" => {
                println!("ionterm {VERSION}");
                return ExitCode::SUCCESS;
            }
            "host" => {
                let Some(host) = args.next() else {
                    eprintln!("Missing host. Usage: ionterm host <host>");
                    return ExitCode::from(2);
                };
                forward = Some(vec!["host".to_string(), host.clone()]);
                config.terminal.startup_host = Some(host);
            }
            "ssh" => {
                let given: Vec<String> = args.by_ref().collect();
                match ssh::detect::parse_args(&given) {
                    Ok(parsed) => {
                        config.terminal.startup_ssh = Some(parsed);
                        forward = Some(std::iter::once("ssh".to_string()).chain(given).collect());
                    }
                    Err(message) => {
                        eprintln!("{message} Usage: ionterm ssh [ssh options] <destination> [command]");
                        return ExitCode::from(2);
                    }
                }
            }
            "-w" | "--new-window" => new_window = true,
            "-e" | "--exec" => {
                config.terminal.command = args.by_ref().collect();
                if config.terminal.command.is_empty() {
                    eprintln!("Missing command after {arg}. Usage: ionterm -e <command> [args...]");
                    return ExitCode::from(2);
                }
            }
            other => {
                eprintln!("Unknown option: {other}. Run \"ionterm --help\" for usage.");
                return ExitCode::from(2);
            }
        }
    }

    if let Some(command) = forward.filter(|_| !new_window) {
        match remote::send(&command) {
            Some(Ok((0, _))) => return ExitCode::SUCCESS,
            // An older ionterm without these commands: open a window instead.
            Some(Ok((_, text))) if text.starts_with("Unknown command") => {}
            Some(Ok((code, text))) => {
                eprintln!("{}", text.trim_end());
                return ExitCode::from(code);
            }
            // No ionterm running, or it went away: open a window instead.
            Some(Err(_)) | None => {}
        }
    }

    let event_loop = match EventLoop::<UserEvent>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(error) => {
            eprintln!("Could not connect to the display server: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut app = App::new(event_loop.create_proxy(), config);
    if let Err(error) = event_loop.run_app(&mut app) {
        eprintln!("ionterm stopped unexpectedly: {error}");
        return ExitCode::FAILURE;
    }
    match app.error {
        Some(error) => {
            eprintln!("ionterm failed to start: {error:#}");
            ExitCode::FAILURE
        }
        None => ExitCode::SUCCESS,
    }
}
