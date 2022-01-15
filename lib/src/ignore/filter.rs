use std::path::{Path, PathBuf};

use futures::stream::{FuturesUnordered, StreamExt};
use ignore::{
	gitignore::{Gitignore, GitignoreBuilder},
	Match,
};
use tokio::fs::read_to_string;
use tracing::{trace, trace_span};

use crate::{
	error::RuntimeError,
	event::{Event, FileType},
	filter::Filterer,
};

use super::files::IgnoreFile;

/// A path-only filterer dedicated to ignore files.
///
/// This reads and compiles ignore files, and should be used for handling ignore files. It's created
/// with a project origin and a list of ignore files, and new ignore files can be added later
/// (unless [`finish`](Filter::finish()) is called).
///
/// It implements [`Filterer`] so it can be used directly in another filterer; it is not designed to
/// be used as a standalone filterer.
#[derive(Debug)]
pub struct IgnoreFilterer {
	origin: PathBuf,
	builder: Option<GitignoreBuilder>,
	compiled: Gitignore,
}

impl IgnoreFilterer {
	/// Read ignore files from disk and load them for filtering.
	pub async fn new(origin: impl AsRef<Path>, files: &[IgnoreFile]) -> Result<Self, RuntimeError> {
		let (files_contents, errors): (Vec<_>, Vec<_>) = files
			.iter()
			.map(|file| async move {
				trace!(?file, "loading ignore file");
				let content = read_to_string(&file.path).await.map_err(|err| {
					RuntimeError::IgnoreFileRead {
						file: file.path.clone(),
						err,
					}
				})?;
				Ok((file.clone(), content))
			})
			.collect::<FuturesUnordered<_>>()
			.collect::<Vec<_>>()
			.await
			.into_iter()
			.map(|res| match res {
				Ok(o) => (Some(o), None),
				Err(e) => (None, Some(e)),
			})
			.unzip();

		let errors: Vec<RuntimeError> = errors.into_iter().flatten().collect();
		if !errors.is_empty() {
			return Err(RuntimeError::Set(errors));
		}

		// TODO: different parser/adapter for non-git-syntax ignore files?

		let origin = origin.as_ref();
		let mut builder = GitignoreBuilder::new(origin);
		for (file, content) in files_contents.into_iter().flatten() {
			let _span = trace_span!("loading ignore file", ?file).entered();
			for line in content.lines() {
				if line.is_empty() || line.starts_with('#') {
					continue;
				}

				trace!(?line, "adding ignore line");
				builder
					.add_line(file.applies_in.clone(), line)
					.map_err(|err| RuntimeError::IgnoreFileGlob {
						file: file.path.clone(),
						err,
					})?;
			}
		}

		trace!("compiling globset");
		let compiled = builder
			.build()
			.map_err(|err| RuntimeError::IgnoreFileGlob {
				file: "set of ignores".into(),
				err,
			})?;

		trace!(
			files=%files.len(),
			ignores=%compiled.num_ignores(),
			allows=%compiled.num_whitelists(),
			"ignore files loaded and compiled",
		);

		Ok(Self {
			origin: origin.to_owned(),
			builder: Some(builder),
			compiled,
		})
	}

	/// Deletes the internal builder, to save memory.
	///
	/// This makes it impossible to add new ignore files without re-compiling the whole set.
	pub fn finish(&mut self) {
		self.builder = None;
	}

	/// Reads and adds an ignore file, if the builder is available.
	///
	/// Does nothing silently otherwise.
	pub async fn add_file(&mut self, file: &IgnoreFile) -> Result<(), RuntimeError> {
		if let Some(ref mut builder) = self.builder {
			trace!(?file, "reading ignore file");
			let content =
				read_to_string(&file.path)
					.await
					.map_err(|err| RuntimeError::IgnoreFileRead {
						file: file.path.clone(),
						err,
					})?;

			let _span = trace_span!("loading ignore file", ?file).entered();
			for line in content.lines() {
				if line.is_empty() || line.starts_with('#') {
					continue;
				}

				trace!(?line, "adding ignore line");
				builder
					.add_line(file.applies_in.clone(), line)
					.map_err(|err| RuntimeError::IgnoreFileGlob {
						file: file.path.clone(),
						err,
					})?;
			}

			let pre_ignores = self.compiled.num_ignores();
			let pre_allows = self.compiled.num_whitelists();

			trace!("recompiling globset");
			let recompiled = builder
				.build()
				.map_err(|err| RuntimeError::IgnoreFileGlob {
					file: file.path.clone(),
					err,
				})?;

			trace!(
				new_ignores=%(recompiled.num_ignores() - pre_ignores),
				new_allows=%(recompiled.num_whitelists() - pre_allows),
				"ignore file loaded and set recompiled",
			);
			self.compiled = recompiled;
		}

		Ok(())
	}

	/// Check a particular folder path against the ignore set.
	///
	/// Returns `false` if the folder should be ignored.
	///
	/// Note that this is a slightly different implementation than the [`Filterer`] trait, as the
	/// latter handles events with multiple associated paths.
	pub fn check_dir(&self, path: &Path) -> bool {
		let _span = trace_span!("check_dir", ?path).entered();

		trace!("checking against compiled ignore files");
		match if path.strip_prefix(&self.origin).is_ok() {
			trace!("checking against path or parents");
			self.compiled.matched_path_or_any_parents(path, true)
		} else {
			trace!("checking against path only");
			self.compiled.matched(path, true)
		} {
			Match::None => {
				trace!("no match (pass)");
				true
			}
			Match::Ignore(glob) => {
				if glob.from().map_or(true, |f| path.strip_prefix(f).is_ok()) {
					trace!(?glob, "positive match (fail)");
					false
				} else {
					trace!(?glob, "positive match, but not in scope (pass)");
					true
				}
			}
			Match::Whitelist(glob) => {
				trace!(?glob, "negative match (pass)");
				true
			}
		}
	}
}

impl Filterer for IgnoreFilterer {
	/// Filter an event.
	///
	/// This implementation never errors. It returns `Ok(false)` if the event is ignored according
	/// to the ignore files, and `Ok(true)` otherwise.
	fn check_event(&self, event: &Event) -> Result<bool, RuntimeError> {
		let _span = trace_span!("filterer_check").entered();
		let mut pass = true;

		for (path, file_type) in event.paths() {
			let _span = trace_span!("path", ?path).entered();
			let is_dir = file_type
				.map(|t| matches!(t, FileType::Dir))
				.unwrap_or(false);

			trace!("checking against compiled ignore files");
			match if path.strip_prefix(&self.origin).is_ok() {
				trace!("checking against path or parents");
				self.compiled.matched_path_or_any_parents(path, is_dir)
			} else {
				trace!("checking against path only");
				self.compiled.matched(path, is_dir)
			} {
				Match::None => {
					trace!("no match (pass)");
					pass &= true;
				}
				Match::Ignore(glob) => {
					if glob.from().map_or(true, |f| path.strip_prefix(f).is_ok()) {
						trace!(?glob, "positive match (fail)");
						pass &= false;
					} else {
						trace!(?glob, "positive match, but not in scope (ignore)");
					}
				}
				Match::Whitelist(glob) => {
					trace!(?glob, "negative match (pass)");
					pass = true;
				}
			}
		}

		trace!(?pass, "verdict");
		Ok(pass)
	}
}
