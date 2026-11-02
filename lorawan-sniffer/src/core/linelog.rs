//! Log lines that a test can read back.
//!
//! A process has one logger, installed by whichever test gets there first,
//! and the tests run in parallel, so a test cannot count on it to have kept
//! the lines its own code logged.  The lines the tests need to see are logged
//! through [`warn_line!`] and [`error_line!`], which behave like `warn!` and
//! `error!` (the line carries the module that logged it) and, when a test has
//! asked with `start_capture`, also keep the line in a buffer private to the
//! test's thread.  That only sees tasks that run on the test's own thread,
//! which is where a `#[tokio::test]` runs everything it spawns.

use std::fmt;

use log::Level;

/// Log one line for `target`, and in tests keep it for the test that is
/// running.  Use the macros, which fill in `target`.
pub(crate) fn line(level: Level, target: &str, args: fmt::Arguments<'_>) {
    log::log!(target: target, level, "{}", args);
    #[cfg(test)]
    capture::record(level, args.to_string());
}

/// Like `warn!`, and readable by a test.
macro_rules! warn_line {
    ($($arg:tt)+) => {
        $crate::core::linelog::line(log::Level::Warn, module_path!(), format_args!($($arg)+))
    };
}

/// Like `error!`, and readable by a test.
macro_rules! error_line {
    ($($arg:tt)+) => {
        $crate::core::linelog::line(log::Level::Error, module_path!(), format_args!($($arg)+))
    };
}

pub(crate) use {error_line, warn_line};

#[cfg(test)]
pub(crate) use capture::{start_capture, take_capture};

#[cfg(test)]
mod capture {
    use std::cell::RefCell;

    use log::Level;

    thread_local! {
        static LINES: RefCell<Option<Vec<(Level, String)>>> = const { RefCell::new(None) };
    }

    pub(super) fn record(level: Level, message: String) {
        LINES.with(|lines| {
            if let Some(lines) = lines.borrow_mut().as_mut() {
                lines.push((level, message));
            }
        });
    }

    /// Start keeping the lines logged on this thread, dropping any kept
    /// before.
    pub(crate) fn start_capture() {
        LINES.with(|lines| *lines.borrow_mut() = Some(Vec::new()));
    }

    /// The lines kept since [`start_capture`], which keeps capturing.
    pub(crate) fn take_capture() -> Vec<(Level, String)> {
        LINES.with(|lines| {
            lines
                .borrow_mut()
                .as_mut()
                .map(std::mem::take)
                .unwrap_or_default()
        })
    }
}
