//! Service log lines on standard error. Under systemd, a `<N>` prefix sets
//! the journal priority (sd-daemon(3)); in a terminal, warnings and errors
//! carry a word instead.

use std::fmt::Display;

#[derive(Clone, Copy)]
enum Level {
	Error,
	Warning,
	Info,
}

fn emit(level: Level, message: &dyn Display) {
	let journal = std::env::var_os("JOURNAL_STREAM").is_some();
	let prefix = match (level, journal) {
		(Level::Error, true) => "<3>",
		(Level::Warning, true) => "<4>",
		(Level::Info, true) => "<6>",
		(Level::Error, false) => "Error: ",
		(Level::Warning, false) => "Warning: ",
		(Level::Info, false) => "",
	};
	eprintln!("{prefix}{message}");
}

pub(crate) fn error(message: impl Display) {
	emit(Level::Error, &message);
}

pub(crate) fn warning(message: impl Display) {
	emit(Level::Warning, &message);
}

pub(crate) fn info(message: impl Display) {
	emit(Level::Info, &message);
}
