//! Path helpers with the `pathlib`/`shutil` semantics the deployment relies on.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};

/// Expands a leading `~` like `Path.expanduser()`. `~user` would need the
/// password database, so it is refused rather than taken relative to the
/// working directory.
pub(crate) fn expand_user(path: &Path, home: &Path) -> Result<PathBuf> {
	match path.components().next() {
		Some(Component::Normal(first)) if first == "~" => {
			Ok(home.join(path.strip_prefix("~").unwrap_or(path)))
		}
		Some(Component::Normal(first)) if first.as_encoded_bytes().starts_with(b"~") => {
			Err(Error::value(format!(
				"{}: ~user paths are not supported; use an absolute path",
				path.display()
			)))
		}
		_ => Ok(path.to_path_buf()),
	}
}

/// Makes `path` absolute without resolving symbolic links, like
/// `Path.absolute()`: `.` components, repeated and trailing slashes go,
/// `..` stays; an empty path is the working directory.
pub(crate) fn absolute(path: &Path) -> io::Result<PathBuf> {
	let path = if path.as_os_str().is_empty() {
		std::env::current_dir()?
	} else {
		std::path::absolute(path)?
	};
	Ok(path.components().collect())
}

/// Resolves symbolic links like `Path.resolve(strict=False)`
/// (`posixpath.realpath`): each component is examined in turn, links are
/// followed even when their target is missing, `..` removes the component
/// before it, and what cannot be examined, including a link loop, stays as
/// written. Fails only when the working directory is unavailable.
pub(crate) fn resolve(path: &Path) -> io::Result<PathBuf> {
	let path = absolute(path)?;
	let mut seen = HashMap::new();
	Ok(join_real(PathBuf::from("/"), &path, &mut seen).0)
}

/// `posixpath._joinrealpath`: appends `rest` to the resolved `base`; the flag
/// is false when a link loop stopped the resolution.
fn join_real(
	mut base: PathBuf,
	rest: &Path,
	seen: &mut HashMap<PathBuf, Option<PathBuf>>,
) -> (PathBuf, bool) {
	let components: Vec<Component> = rest.components().collect();
	for (index, component) in components.iter().enumerate() {
		let name = match component {
			Component::RootDir | Component::Prefix(_) => {
				base = PathBuf::from("/");
				continue;
			}
			Component::CurDir => continue,
			Component::ParentDir => {
				base.pop();
				continue;
			}
			Component::Normal(name) => name,
		};
		let next = base.join(name);
		let is_link = fs::symlink_metadata(&next).is_ok_and(|meta| meta.file_type().is_symlink());
		if !is_link {
			base = next;
			continue;
		}
		let remaining = || components[index + 1..].iter().collect::<PathBuf>();
		match seen.get(&next) {
			Some(Some(resolved)) => {
				base = resolved.clone();
				continue;
			}
			// Still being resolved: a loop.
			Some(None) => return (next.join(remaining()), false),
			None => {}
		}
		let Ok(target) = fs::read_link(&next) else {
			base = next;
			continue;
		};
		seen.insert(next.clone(), None);
		let (resolved, complete) = join_real(base, &target, seen);
		if !complete {
			return (resolved.join(remaining()), false);
		}
		seen.insert(next, Some(resolved.clone()));
		base = resolved;
	}
	(base, true)
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
	use std::os::unix::fs::symlink;

	use super::*;

	/// A canonical scratch directory, removed on drop.
	struct Scratch(PathBuf);

	impl Scratch {
		fn new(name: &str) -> Self {
			let root = fs::canonicalize(std::env::temp_dir())
				.unwrap()
				.join(format!("devtunnel-paths-{name}-{}", std::process::id()));
			fs::create_dir_all(&root).unwrap();
			Self(root)
		}
	}

	impl Drop for Scratch {
		fn drop(&mut self) {
			fs::remove_dir_all(&self.0).ok();
		}
	}

	#[test]
	fn expands_home() {
		let home = Path::new("/home/u");
		assert_eq!(
			expand_user(Path::new("~/bin/x"), home).unwrap(),
			Path::new("/home/u/bin/x")
		);
		assert_eq!(
			expand_user(Path::new("~"), home).unwrap(),
			Path::new("/home/u")
		);
		assert_eq!(
			expand_user(Path::new("/a/~"), home).unwrap(),
			Path::new("/a/~")
		);
		assert_eq!(
			expand_user(Path::new("a/b"), home).unwrap(),
			Path::new("a/b")
		);
		assert_eq!(
			expand_user(Path::new("~alice/bin"), home)
				.unwrap_err()
				.to_string(),
			"ValueError: ~alice/bin: ~user paths are not supported; use an absolute path"
		);
	}

	#[test]
	fn absolute_normalizes_like_pathlib() {
		assert_eq!(
			absolute(Path::new("/a//b/./c/")).unwrap(),
			Path::new("/a/b/c")
		);
		assert_eq!(
			absolute(Path::new("/a/../b")).unwrap(),
			Path::new("/a/../b")
		);
		assert_eq!(
			absolute(Path::new("")).unwrap(),
			std::env::current_dir().unwrap()
		);
	}

	#[test]
	fn resolves_like_posixpath_realpath() {
		// Expectations checked against Python 3.14's Path.resolve().
		let scratch = Scratch::new("realpath");
		let root = &scratch.0;
		fs::create_dir_all(root.join("real/dir")).unwrap();
		symlink(root.join("real"), root.join("link")).unwrap();
		symlink("dotfiles/config", root.join("dangling")).unwrap();
		symlink(root.join("loop-b"), root.join("loop-a")).unwrap();
		symlink(root.join("loop-a"), root.join("loop-b")).unwrap();
		symlink("../real", root.join("real/dir/up")).unwrap();
		for (input, expected) in [
			("link/dir", "real/dir"),
			("link/dir/missing/x", "real/dir/missing/x"),
			// A dangling link resolves to its missing target.
			(
				"dangling/devtunnel-service",
				"dotfiles/config/devtunnel-service",
			),
			// `..` after a missing component removes it.
			("missing/x/../cfg", "missing/cfg"),
			// A relative target is read against the link's directory.
			("link/dir/up/dir", "real/real/dir"),
			// A loop stays as written from the looping link on.
			("loop-a/x", "loop-a/x"),
			("real/dir/", "real/dir"),
		] {
			assert_eq!(
				resolve(&root.join(input)).unwrap(),
				root.join(expected),
				"{input}"
			);
		}
		assert_eq!(resolve(Path::new("/..")).unwrap(), Path::new("/"));
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
