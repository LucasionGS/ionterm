//! Desktop notifications, through `notify-send` (libnotify) on Linux.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Programs can't flood the desktop: at most one notification this often.
const MIN_INTERVAL: Duration = Duration::from_secs(1);

/// Shows a notification. Quietly does nothing where there's no way to.
pub fn send(title: &str, body: &str) {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if last.is_some_and(|at| at.elapsed() < MIN_INTERVAL) {
            return;
        }
        *last = Some(Instant::now());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let title = if title.trim().is_empty() { "ionterm" } else { title };
        let spawned = std::process::Command::new("notify-send")
            .args(["--app-name=ionterm", "--icon=ionterm", "--", title, body])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Err(error) = spawned {
            tracing::debug!("no notification ({error}): {title}: {body}");
        }
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    {
        let _ = (title, body);
    }
}

/// A duration as people say it: "45s", "2m 03s", "1h 05m".
pub fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_durations() {
        assert_eq!(duration(Duration::from_secs(9)), "9s");
        assert_eq!(duration(Duration::from_secs(123)), "2m 03s");
        assert_eq!(duration(Duration::from_secs(3900)), "1h 05m");
    }
}
