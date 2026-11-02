//! Helpers shared by tests in different modules.

use std::sync::{Mutex, OnceLock};

/// Collects every log line, at every level, from the whole test process.
pub(crate) struct CaptureLogger {
    pub(crate) lines: Mutex<Vec<String>>,
}

impl log::Log for CaptureLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        self.lines.lock().unwrap().push(format!(
            "{} {}: {}",
            record.level(),
            record.target(),
            record.args()
        ));
    }

    fn flush(&self) {}
}

/// The process-wide capturing logger, installed (at TRACE) on first use.
///
/// A process has one logger, so every test that wants the lines of its own
/// run installs the same one and filters what it reads by what it logged.
pub(crate) fn capture_logger() -> &'static CaptureLogger {
    static LOGGER: OnceLock<CaptureLogger> = OnceLock::new();
    let logger = LOGGER.get_or_init(|| CaptureLogger {
        lines: Mutex::new(Vec::new()),
    });
    // Ignore the error from a second test installing it again.
    let _ = log::set_logger(logger);
    log::set_max_level(log::LevelFilter::Trace);
    logger
}
