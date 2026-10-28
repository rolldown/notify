use std::path::Path;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use napi::{Error, Result, Status};

mod watcher;

struct IgnoredGlobs {
    included: GlobSet,
    excluded: GlobSet,
}

impl IgnoredGlobs {
    fn is_empty(&self) -> bool {
        self.included.is_empty()
    }

    fn has_exclusions(&self) -> bool {
        !self.excluded.is_empty()
    }

    fn is_included(&self, path: impl AsRef<Path>) -> bool {
        self.included.is_match(path)
    }

    fn is_excluded(&self, path: impl AsRef<Path>) -> bool {
        self.excluded.is_match(path)
    }

    fn is_match(&self, path: impl AsRef<Path>) -> bool {
        let path = path.as_ref();
        self.is_included(path) && !self.is_excluded(path)
    }
}

fn ignored_globs(patterns: &[String]) -> Result<IgnoredGlobs> {
    let mut included = GlobSetBuilder::new();
    let mut excluded = GlobSetBuilder::new();
    for pattern in patterns {
        let (builder, pattern) = if let Some(pattern) = pattern.strip_prefix('!') {
            (&mut excluded, pattern)
        } else {
            (&mut included, pattern.as_str())
        };
        add_glob(builder, pattern)?;
        if let Some(directory) = pattern.strip_suffix("/**") {
            add_glob(builder, directory)?;
        }
    }
    Ok(IgnoredGlobs {
        included: build_globs(&included)?,
        excluded: build_globs(&excluded)?,
    })
}

fn add_glob(builder: &mut GlobSetBuilder, pattern: &str) -> Result<()> {
    let glob = GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|error| Error::new(Status::InvalidArg, error.to_string()))?;
    builder.add(glob);
    Ok(())
}

fn build_globs(builder: &GlobSetBuilder) -> Result<GlobSet> {
    builder
        .build()
        .map_err(|error| Error::new(Status::InvalidArg, error.to_string()))
}

fn path_string(path: impl AsRef<Path>) -> String {
    path.as_ref().to_string_lossy().into_owned()
}

fn notify_error(error: &notify::Error) -> Error {
    Error::new(Status::GenericFailure, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignored_globs_support_negation() {
        let globs =
            ignored_globs(&["ignored/**".to_string(), "!ignored/keep/**".to_string()]).unwrap();

        assert!(globs.is_match("ignored"));
        assert!(globs.is_match("ignored/drop.js"));
        assert!(!globs.is_match("ignored/keep"));
        assert!(!globs.is_match("ignored/keep/file.js"));
        assert!(!globs.is_match("visible.js"));
    }

    #[test]
    fn negation_without_an_inclusion_ignores_nothing() {
        let globs = ignored_globs(&["!keep/**".to_string()]).unwrap();

        assert!(!globs.is_match("drop/file.js"));
        assert!(!globs.is_match("keep/file.js"));
    }
}
