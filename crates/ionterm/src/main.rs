//! ionterm: a GPU-rendered terminal emulator for the Ionix desktop.

mod app;
mod config;
mod input;
mod pty;
mod theme;

use std::process::ExitCode;

use tracing_subscriber::EnvFilter;
use winit::event_loop::EventLoop;

use crate::app::{App, UserEvent};
use crate::config::Config;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_help() {
    println!("ionterm {VERSION}: a GPU-rendered terminal emulator\n");
    println!("Usage: ionterm [options] [-e <command> [args...]]\n");
    println!("Options:");
    println!("  -e, --exec <command>   Run <command> instead of the shell (takes the rest of the line)");
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

    let mut config = Config::load();
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
