//! Path helpers with the `pathlib`/`shutil` semantics the deployment relies on.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Expands a leading `~` like `Path.expanduser()`.
pub(crate) fn expand_user(path: &Path, home: &Path) -> PathBuf {
	match path.strip_prefix("~") {
		Ok(rest) => home.join(rest),
		Err(_) => path.to_path_buf(),
	}
}

/// Makes `path` absolute without resolving symbolic links, like
/// `Path.absolute()`.
pub(crate) fn absolute(path: &Path) -> io::Result<PathBuf> {
	std::path::absolute(path)
}

/// Resolves symbolic links as far as the path exists, like
/// `Path.resolve(strict=False)`.
pub(crate) fn resolve(path: &Path) -> io::Result<PathBuf> {
	let path = absolute(path)?;
	match fs::canonicalize(&path) {
		Ok(resolved) => Ok(resolved),
		Err(error)
			if matches!(
				error.kind(),
				io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
			) =>
		{
			match (path.parent(), path.file_name()) {
				(Some(parent), Some(name)) => Ok(resolve(parent)?.join(name)),
				_ => Ok(path),
			}
		}
		Err(error) => Err(error),
	}
}

/// Whether `path` is a file someone may execute, approximating
/// `os.access(path, os.X_OK)` for unprivileged users.
pub(crate) fn is_executable(path: &Path) -> bool {
	fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// The first executable `program` in `search_path`, like `shutil.which`.
pub(crate) fn which(program: &str, search_path: Option<&OsStr>) -> Option<PathBuf> {
	let search_path = search_path.unwrap_or(OsStr::new("/bin:/usr/bin"));
	if search_path.is_empty() {
		return None;
	}
	std::env::split_paths(search_path)
		.map(|dir| dir.join(program))
		.find(|candidate| is_executable(candidate))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn expands_home() {
		let home = Path::new("/home/u");
		assert_eq!(
			expand_user(Path::new("~/bin/x"), home),
			Path::new("/home/u/bin/x")
		);
		assert_eq!(expand_user(Path::new("~"), home), Path::new("/home/u"));
		assert_eq!(expand_user(Path::new("/a/~"), home), Path::new("/a/~"));
	}

	#[test]
	fn resolves_missing_tail() {
		let root = fs::canonicalize(std::env::temp_dir()).unwrap();
		let missing = root.join("devtunnel-service-missing/a/b");
		assert_eq!(resolve(&missing).unwrap(), missing);
	}

	#[test]
	fn which_finds_executables_only() {
		assert_eq!(
			which("sh", Some(OsStr::new("/nonexistent:/bin"))),
			Some(PathBuf::from("/bin/sh"))
		);
		assert_eq!(which("sh", Some(OsStr::new(""))), None);
	}
}
