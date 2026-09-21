//! Local settings: what a job needs that is not in the repository and not in
//! the input.
//!
//! One YAML file, outside the source tree, holding what belongs to the machine
//! rather than to the project. Today that is one thing — the key an Evolution
//! frame's protection field is signed with, which is not this project's to
//! ship. Read from, in order:
//!
//! 1. the path given on the command line (`--config`);
//! 2. `$HARLETTIZER_CONFIG`;
//! 3. `$XDG_CONFIG_HOME/harlettizer/config.yaml`;
//! 4. `~/.config/harlettizer/config.yaml`.
//!
//! An explicit path that does not exist is an error; a default one that does
//! not exist is simply no settings. The shape is strict — an unknown key fails
//! rather than being ignored — because a misspelt key that is silently dropped
//! is a stream written unsigned without anyone being told.
//!
//! ```yaml
//! # ~/.config/harlettizer/config.yaml — local to this machine.
//! evolution_key: "0001…"   # hexadecimal, 32 bytes for the key streams carry
//! ```

use hz_core::{Error, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The file's name under its directory.
const FILE: &str = "config.yaml";

/// The environment variable that names the file outright.
const ENV: &str = "HARLETTIZER_CONFIG";

/// What the file says, decoded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    /// The key Evolution frames' protection fields are signed with.
    pub evolution_key: Option<Vec<u8>>,
    /// Where it was read from, when it was.
    pub from: Option<PathBuf>,
}

/// The file's shape as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    evolution_key: Option<String>,
}

impl Settings {
    /// Where the settings would be read from, given what the command line
    /// said: the first location in the order above that is named at all,
    /// whether or not it exists.
    pub fn location(explicit: Option<&Path>) -> Option<PathBuf> {
        if let Some(path) = explicit {
            return Some(path.to_path_buf());
        }
        if let Some(path) = std::env::var_os(ENV).filter(|path| !path.is_empty()) {
            return Some(PathBuf::from(path));
        }
        let directory = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(directory.join("harlettizer").join(FILE))
    }

    /// Read the settings, from the explicit path or the first default one.
    ///
    /// A default file that is absent is no settings and not an error; a path
    /// that was asked for by name has to exist.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let Some(path) = Self::location(explicit) else {
            return Ok(Self::default());
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(why) if why.kind() == std::io::ErrorKind::NotFound && explicit.is_none() => {
                return Ok(Self::default());
            }
            Err(why) => return Err(Error::io(&path, why)),
        };
        let mut settings = Self::parse(&text).map_err(|why| Error::malformed(&path, why))?;
        settings.from = Some(path);
        Ok(settings)
    }

    pub fn parse(text: &str) -> std::result::Result<Self, String> {
        let file: File = serde_yaml_ng::from_str(text).map_err(|why| why.to_string())?;
        let evolution_key = file
            .evolution_key
            .map(|hex| unhex(&hex).map_err(|why| format!("evolution_key: {why}")))
            .transpose()?;
        Ok(Self {
            evolution_key,
            from: None,
        })
    }
}

/// Hexadecimal, whitespace allowed anywhere, to bytes.
fn unhex(text: &str) -> std::result::Result<Vec<u8>, String> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .map(|byte| {
            (byte as char)
                .to_digit(16)
                .map(|digit| digit as u8)
                .ok_or_else(|| format!("`{}` is not a hexadecimal digit", byte as char))
        })
        .collect::<std::result::Result<_, _>>()?;
    if digits.is_empty() {
        return Err("empty".into());
    }
    if digits.len() % 2 != 0 {
        return Err(format!(
            "{} hexadecimal digits is not a whole number of bytes",
            digits.len()
        ));
    }
    Ok(digits
        .chunks_exact(2)
        .map(|pair| pair[0] << 4 | pair[1])
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_reads_as_bytes_whatever_its_spacing() {
        let settings = Settings::parse("evolution_key: \"00 01 0a\\n ff\"\n").unwrap();
        assert_eq!(settings.evolution_key, Some(vec![0x00, 0x01, 0x0a, 0xff]));
        let settings = Settings::parse("evolution_key: 0001FF\n").unwrap();
        assert_eq!(settings.evolution_key, Some(vec![0x00, 0x01, 0xff]));
    }

    #[test]
    fn an_empty_file_is_no_settings() {
        assert_eq!(Settings::parse("").unwrap(), Settings::default());
        assert_eq!(
            Settings::parse("# only a comment\n").unwrap(),
            Settings::default()
        );
    }

    #[test]
    fn a_malformed_key_is_refused_and_says_why() {
        let why = Settings::parse("evolution_key: abc\n").unwrap_err();
        assert!(why.contains("evolution_key"), "{why}");
        assert!(why.contains("whole number of bytes"), "{why}");
        let why = Settings::parse("evolution_key: xyz0\n").unwrap_err();
        assert!(why.contains("not a hexadecimal digit"), "{why}");
        let why = Settings::parse("evolution_key: \"\"\n").unwrap_err();
        assert!(why.contains("empty"), "{why}");
    }

    /// A key spelt wrong is a stream written unsigned; the file refuses it.
    #[test]
    fn an_unknown_key_is_refused() {
        assert!(Settings::parse("evolution-key: 00\n").is_err());
        assert!(Settings::parse("evolution_key: 00\nother: 1\n").is_err());
    }

    #[test]
    fn an_explicit_path_wins_and_has_to_exist() {
        let path = Path::new("/nonexistent/harlettizer/settings.yaml");
        assert_eq!(Settings::location(Some(path)), Some(path.to_path_buf()));
        assert!(Settings::load(Some(path)).is_err());
    }
}
