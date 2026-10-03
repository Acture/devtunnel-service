//! Service log lines on standard error. When standard error is the journal,
//! a `<N>` prefix sets the priority (sd-daemon(3)); in a terminal, warnings
//! carry a word instead.

use std::fmt::Display;
use std::sync::OnceLock;

#[derive(Clone, Copy)]
enum Level {
	Failure,
	Warning,
	Info,
}

/// Whether standard error is the journal stream systemd connected, as
/// `JOURNAL_STREAM` (`DEVICE:INODE`) identifies it. Children of a service,
/// such as shells in a terminal it started, inherit the variable with a
/// different standard error.
fn journal() -> bool {
	static JOURNAL: OnceLock<bool> = OnceLock::new();
	*JOURNAL.get_or_init(|| {
		use std::os::fd::AsFd;
		use std::os::unix::fs::MetadataExt;
		let Some(expected) = std::env::var_os("JOURNAL_STREAM") else {
			return false;
		};
		std::io::stderr()
			.as_fd()
			.try_clone_to_owned()
			.and_then(|fd| std::fs::File::from(fd).metadata())
			.is_ok_and(|meta| expected.to_str() == Some(&format!("{}:{}", meta.dev(), meta.ino())))
	})
}

fn emit(level: Level, message: &dyn Display) {
	let prefix = match (level, journal()) {
		(Level::Failure, true) => "<3>",
		(Level::Warning, true) => "<4>",
		(Level::Info, true) => "<6>",
		(Level::Warning, false) => "Warning: ",
		(Level::Failure | Level::Info, false) => "",
	};
	eprintln!("{prefix}{message}");
}

/// The one-line error that ends the process; its text carries no prefix.
pub(crate) fn failure(message: impl Display) {
	emit(Level::Failure, &message);
}

pub(crate) fn warning(message: impl Display) {
	emit(Level::Warning, &message);
}

pub(crate) fn info(message: impl Display) {
	emit(Level::Info, &message);
}
