//! Whether two paths name one file.
//!
//! Kept out of the commands that use it so that it is compiled, and its tests
//! run, in every build — the harness's oracle commands are behind features no
//! CI job turns on, and the case that matters most here is the one only a
//! Windows runner can see.

use hz_core::{Error, Result};
use std::path::Path;

/// Whether `out` names the file `input` is, by any path: the same path, one
/// through `.` and `..` or a symbolic link, or another hard link to it — on
/// Unix the same device and inode, on Windows the same volume and file index.
/// A command that writes `out` before it has finished reading `input` has to
/// refuse such an `out`, or it destroys what it reads.
///
/// `input` must exist; `out` need not, and an `out` that does not exist is
/// still caught when its directory, through links, is `input`'s and its name
/// is `input`'s name.
#[cfg_attr(not(feature = "eac3-oracle"), allow(dead_code))]
pub fn aliases(input: &Path, out: &Path) -> Result<bool> {
    let input_at = std::fs::canonicalize(input).map_err(|e| Error::io(input, e))?;
    if out.exists() {
        // Volume and file identity, which a hard link shares and its path
        // does not.
        return same_file::is_same_file(input, out).map_err(|e| Error::io(out, e));
    }
    let parent = match out.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    Ok(match (std::fs::canonicalize(parent), out.file_name()) {
        (Ok(dir), Some(name)) => dir.join(name) == input_at,
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A fresh directory of its own for each test, removed when it ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("hz-xtask-paths-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, b"reference").unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_same_file_by_its_own_path_and_another_spelling() {
        let scratch = Scratch::new("spelling");
        let input = scratch.file("reference.ec3");
        assert!(aliases(&input, &input).unwrap());
        let spelled = scratch.0.join(".").join("reference.ec3");
        assert!(aliases(&input, &spelled).unwrap());
        let up = scratch
            .0
            .join("..")
            .join(scratch.0.file_name().unwrap())
            .join("reference.ec3");
        assert!(aliases(&input, &up).unwrap());
    }

    /// The case the review found open on Windows: a second name for the same
    /// file, which no canonical path makes equal to the first.
    #[test]
    fn a_hard_link_is_the_same_file() {
        let scratch = Scratch::new("hard");
        let input = scratch.file("reference.ec3");
        let link = scratch.0.join("alias.ec3");
        std::fs::hard_link(&input, &link).unwrap();
        assert!(aliases(&input, &link).unwrap());
        assert!(aliases(&link, &input).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_is_the_same_file() {
        let scratch = Scratch::new("symbolic");
        let input = scratch.file("reference.ec3");
        let link = scratch.0.join("alias.ec3");
        std::os::unix::fs::symlink(&input, &link).unwrap();
        assert!(aliases(&input, &link).unwrap());
    }

    #[test]
    fn another_file_or_a_new_one_is_not() {
        let scratch = Scratch::new("other");
        let input = scratch.file("reference.ec3");
        let other = scratch.file("other.ec3");
        assert!(!aliases(&input, &other).unwrap());
        assert!(!aliases(&input, &scratch.0.join("grafted.ec3")).unwrap());
        // A copy has the same bytes and is a different file.
        let copy = scratch.0.join("copy.ec3");
        std::fs::copy(&input, &copy).unwrap();
        assert!(!aliases(&input, &copy).unwrap());
    }
}
