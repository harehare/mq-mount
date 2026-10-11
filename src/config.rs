//! `.mq-mount.toml`: default values for mount options, so a project can
//! pin how its docs are mounted (`toc = true`, `exclude = [...]`, ...)
//! instead of repeating the same flags on every run. Command-line flags
//! always win over the file; see [`Config::discover`] for where it's
//! looked up.

use std::path::{Path, PathBuf};

use serde::Deserialize;

pub const FILE_NAME: &str = ".mq-mount.toml";

/// Every key is optional and mirrors the long flag of the same name.
/// Unknown keys are rejected, so a typo is reported instead of ignored.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    pub write: Option<bool>,
    pub allow_other: Option<bool>,
    pub watch: Option<bool>,
    pub toc: Option<bool>,
    pub full: Option<bool>,
    pub backup: Option<bool>,
    pub slug: Option<String>,
    pub on_conflict: Option<String>,
    pub filter: Option<String>,
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
}

impl Config {
    pub fn parse(text: &str, path: &Path) -> miette::Result<Self> {
        toml::from_str(text).map_err(|e| miette::miette!("invalid {}: {e}", path.display()))
    }

    pub fn load(path: &Path) -> miette::Result<Self> {
        let text =
            std::fs::read_to_string(path).map_err(|e| miette::miette!("failed to read {}: {e}", path.display()))?;
        Self::parse(&text, path)
    }

    /// The config file to use when none is given explicitly: `FILE_NAME` in
    /// the current directory, else in the first source's directory (the
    /// source itself for a directory argument, its parent for a file).
    pub fn discover(cwd: &Path, sources: &[PathBuf]) -> Option<PathBuf> {
        let source_dir = sources
            .first()
            .and_then(|s| if s.is_dir() { Some(s.as_path()) } else { s.parent() });
        std::iter::once(cwd)
            .chain(source_dir)
            .map(|dir| dir.join(FILE_NAME))
            .find(|p| p.is_file())
    }

    /// Parses an enum-valued key (`slug`, `on-conflict`) with the same
    /// spelling its command-line flag accepts.
    pub fn value_enum<T: clap::ValueEnum>(value: Option<&str>, key: &str) -> miette::Result<Option<T>> {
        value
            .map(|v| {
                T::from_str(v, false).map_err(|_| {
                    let allowed: Vec<String> = T::value_variants()
                        .iter()
                        .filter_map(|v| v.to_possible_value().map(|p| p.get_name().to_string()))
                        .collect();
                    miette::miette!(
                        "invalid {FILE_NAME} value {v:?} for {key} (expected one of: {})",
                        allowed.join(", ")
                    )
                })
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::SlugStyle;

    #[test]
    fn parses_every_key() {
        let config = Config::parse(
            r#"
write = true
allow-other = false
watch = true
toc = true
full = true
backup = true
slug = "kebab"
on-conflict = "reject"
filter = ".h2"
include = ["guide/**/*.md"]
exclude = ["**/draft-*.md"]
"#,
            Path::new(FILE_NAME),
        )
        .unwrap();
        assert_eq!(config.write, Some(true));
        assert_eq!(config.allow_other, Some(false));
        assert_eq!(config.slug.as_deref(), Some("kebab"));
        assert_eq!(config.include, Some(vec!["guide/**/*.md".to_string()]));
        assert_eq!(
            Config::value_enum::<SlugStyle>(config.slug.as_deref(), "slug").unwrap(),
            Some(SlugStyle::Kebab)
        );
    }

    #[test]
    fn rejects_unknown_keys_and_bad_enum_values() {
        assert!(Config::parse("tocc = true", Path::new(FILE_NAME)).is_err());
        assert!(Config::value_enum::<SlugStyle>(Some("snake"), "slug").is_err());
    }

    #[test]
    fn an_empty_file_sets_nothing() {
        assert_eq!(Config::parse("", Path::new(FILE_NAME)).unwrap(), Config::default());
    }

    #[test]
    fn discover_prefers_the_current_directory_then_the_first_source() {
        let cwd = tempfile::tempdir().unwrap();
        let docs = tempfile::tempdir().unwrap();
        let file = docs.path().join("a.md");
        std::fs::write(&file, "# A\n").unwrap();

        assert_eq!(Config::discover(cwd.path(), std::slice::from_ref(&file)), None);

        std::fs::write(docs.path().join(FILE_NAME), "").unwrap();
        assert_eq!(
            Config::discover(cwd.path(), std::slice::from_ref(&file)),
            Some(docs.path().join(FILE_NAME))
        );
        assert_eq!(
            Config::discover(cwd.path(), &[docs.path().to_path_buf()]),
            Some(docs.path().join(FILE_NAME))
        );

        std::fs::write(cwd.path().join(FILE_NAME), "").unwrap();
        assert_eq!(Config::discover(cwd.path(), &[file]), Some(cwd.path().join(FILE_NAME)));
    }
}
