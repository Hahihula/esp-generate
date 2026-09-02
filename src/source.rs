//! Where a template's files come from.
//!
//! Every consumer goes through [`TemplateSource`] rather than touching
//! [`TEMPLATE_FILES`], so adding a source that isn't compiled in — a directory,
//! an unpacked archive — is a change to this module alone.
//!
//! Paths are template-root-relative throughout: the key space `build.rs` emits,
//! that `!Include` and `include` resolve against, and that a generated file
//! lands on before `emit.as` gets a say.

use crate::template_files::TEMPLATE_FILES;

/// A template's file set.
///
/// Only [`Bundled`](TemplateSource::Bundled) exists today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateSource {
    /// The template compiled into this binary by `build.rs`.
    Bundled,
}

impl TemplateSource {
    /// The contents of `path`, or `None` if this template has no such file.
    pub fn get(&self, path: &str) -> Option<&'static str> {
        match self {
            TemplateSource::Bundled => TEMPLATE_FILES
                .iter()
                .find_map(|(k, v)| (*k == path).then_some(*v)),
        }
    }

    /// The contents of `path`, or an error naming it.
    pub fn read(&self, path: &str) -> Result<&'static str, String> {
        self.get(path)
            .ok_or_else(|| format!("template has no file `{path}`"))
    }

    /// Every `(path, contents)` pair, in the order the source lists them —
    /// which is the order generation writes them in.
    pub fn files(&self) -> impl Iterator<Item = (&'static str, &'static str)> {
        match self {
            TemplateSource::Bundled => TEMPLATE_FILES.iter().map(|(k, v)| (*k, *v)),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn the_bundled_source_resolves_root_relative_paths() {
        let source = TemplateSource::Bundled;

        assert!(source.get("template.yaml").is_some());
        assert!(source.get("src/bin/main.rs").is_some());
        // Nested and dotfile paths use the same key space.
        assert!(source.get(".template/module.yaml").is_some());
        assert!(source.get(".nvim.lua").is_some());

        // `plugin:chip` comes from a plugin, so it is deliberately not a file.
        assert_eq!(source.get("plugin:chip"), None);

        assert_eq!(source.get("no/such/file"), None);
    }

    #[test]
    fn read_names_the_missing_path() {
        let err = TemplateSource::Bundled
            .read("no/such/file")
            .expect_err("a missing file must be an error, not empty content");
        assert!(err.contains("no/such/file"), "{err}");
    }

    #[test]
    fn files_agrees_with_get() {
        let source = TemplateSource::Bundled;
        let files: Vec<_> = source.files().collect();

        assert!(!files.is_empty(), "the bundled template is not empty");
        for (path, contents) in files {
            assert_eq!(
                source.get(path),
                Some(contents),
                "`{path}` from `files()` must resolve through `get()`"
            );
        }
    }
}
