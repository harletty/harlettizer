//! Whether two paths name one file.
//!
//! A command that writes its output while it still reads its input has to
//! refuse an output that is the input under another name, or it truncates
//! what it is reading. Here rather than beside the commands so that every
//! one of them — the command line's and the harness's — asks the same
//! question the same way, and the tests run in every build: the case that
//! matters most is one only a Windows runner can see.

use hz_core::{Error, Result};
use std::path::{Path, PathBuf};

/// Whether `out` names the file `input` is, by any path: the same path, one
/// through `.` and `..` or a symbolic link, or another hard link to it — on
/// Unix the same device and inode, on Windows the same volume and file index.
/// A command that writes `out` before it has finished reading `input` has to
/// refuse such an `out`, or it destroys what it reads.
///
/// `input` must exist; `out` need not, and an `out` that does not exist is
/// still caught when its directory, through links, is `input`'s and its name
/// is `input`'s name.
pub fn aliases(input: &Path, out: &Path) -> Result<bool> {
    let input_at = std::fs::canonicalize(input).map_err(|e| Error::io(input, e))?;
    if out.exists() {
        // Volume and file identity, which a hard link shares and its path
        // does not.
        return one_file(input, out);
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

/// Whether two existing paths are one file, asked without opening either:
/// an output may be a FIFO a reader is waiting on, which another open would
/// block on, or a file its owner may write and not read.
#[cfg(unix)]
fn one_file(input: &Path, out: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    // `metadata` follows symbolic links, as the identity must.
    let identity = |path: &Path| {
        std::fs::metadata(path)
            .map(|meta| (meta.dev(), meta.ino()))
            .map_err(|e| Error::io(path, e))
    };
    Ok(identity(input)? == identity(out)?)
}

/// Whether two existing paths are one file: the volume serial and file
/// index, which `same-file` reads through a handle opened with no access
/// rights, so a file that cannot be read is still compared.
#[cfg(not(unix))]
fn one_file(input: &Path, out: &Path) -> Result<bool> {
    same_file::is_same_file(input, out).map_err(|e| Error::io(out, e))
}

/// Refuse `out` when it is any of `inputs` by any name ([`aliases`]):
/// creating it would truncate what the command is still to read.
///
/// Called before the output is created, with every file the input is read
/// from — a master set's config and components, its mono waveforms, a BW64
/// file — since any of them may be the one a hard link points at.
pub fn refuse_inputs(inputs: &[PathBuf], out: &Path) -> Result<()> {
    for input in inputs {
        if aliases(input, out)? {
            return Err(Error::refused(
                out,
                format!(
                    "is {}, which this reads: writing it would destroy the input",
                    input.display()
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory of its own for each test, removed when it ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("hz-io-paths-{name}-{}", std::process::id()));
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

    /// A master set's files, the way a command hands them over.
    fn master(scratch: &Scratch) -> Vec<PathBuf> {
        [
            "programme.atmos",
            "programme.atmos.metadata",
            "programme.atmos.audio",
        ]
        .map(|name| scratch.file(name))
        .to_vec()
    }

    #[test]
    fn an_output_hard_linked_to_any_input_is_refused() {
        let scratch = Scratch::new("refuse-hard");
        let inputs = master(&scratch);
        let link = scratch.0.join("out.ac3");
        std::fs::hard_link(&inputs[2], &link).unwrap();
        let refused = refuse_inputs(&inputs, &link).unwrap_err();
        assert!(matches!(refused, Error::Refused { .. }), "{refused}");
        // Refused, not opened: the input still holds what it held.
        assert_eq!(std::fs::read(&inputs[2]).unwrap(), b"reference");
    }

    #[cfg(unix)]
    #[test]
    fn an_output_symlinked_to_any_input_is_refused() {
        let scratch = Scratch::new("refuse-symbolic");
        let inputs = master(&scratch);
        let link = scratch.0.join("out.iamf");
        std::os::unix::fs::symlink(&inputs[0], &link).unwrap();
        assert!(refuse_inputs(&inputs, &link).is_err());
        assert!(refuse_inputs(&inputs, &inputs[1]).is_err());
    }

    #[test]
    fn an_unrelated_output_is_not_refused() {
        let scratch = Scratch::new("refuse-other");
        let inputs = master(&scratch);
        let other = scratch.file("out.ec3");
        refuse_inputs(&inputs, &other).unwrap();
        refuse_inputs(&inputs, &scratch.0.join("new.ec3")).unwrap();
    }

    /// An output that is a FIFO is a stream to a reader, never an input; it
    /// is told apart without being opened, which would block until a writer
    /// came.
    #[cfg(unix)]
    #[test]
    fn a_fifo_output_is_not_opened() {
        let scratch = Scratch::new("refuse-fifo");
        let inputs = master(&scratch);
        let fifo = scratch.0.join("out.ac3");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || sent.send(refuse_inputs(&inputs, &fifo).is_ok()));
        let answered = received.recv_timeout(std::time::Duration::from_secs(10));
        assert_eq!(answered, Ok(true), "the check blocked on the FIFO");
    }

    /// An output its owner may write and not read is still compared, and an
    /// unrelated one is let through.
    #[cfg(unix)]
    #[test]
    fn a_write_only_output_is_compared() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = Scratch::new("refuse-write-only");
        let inputs = master(&scratch);
        let other = scratch.file("out.ec3");
        std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o200)).unwrap();
        refuse_inputs(&inputs, &other).unwrap();
        std::fs::set_permissions(&inputs[2], std::fs::Permissions::from_mode(0o200)).unwrap();
        let link = scratch.0.join("out.ac3");
        std::fs::hard_link(&inputs[2], &link).unwrap();
        assert!(refuse_inputs(&inputs, &link).is_err());
    }
}
