//! Round-trip every master set under a root through the reader and writer.
//!
//! This is the phase 1 acceptance test, and it lives here rather than in the
//! test suite for two reasons: it needs real master sets, which CI does not
//! have, and it reads two gigabytes of YAML, which no `cargo test` should.
//!
//! What it proves, and what it does not:
//!
//! * **Semantic round-trip** — parse, write, parse again, and the two models
//!   must be equal. On its own this proves less than it looks: a field the
//!   model does not know about is dropped by the first parse and is therefore
//!   equal to itself afterwards.
//! * **Strictness** — which is what closes that hole. Every struct in `hz-io`
//!   is `deny_unknown_fields`, so a key we failed to model is a hard read
//!   failure, not silent loss. Run over real masters, the two together are a
//!   completeness proof.
//! * **Byte fidelity** — reported, never required. The masters seen were
//!   written by an older tool with a different layout, so chasing byte
//!   equality would be testing someone else's formatting rather than our
//!   model.

use hz_core::{Error, Result};
use hz_io::container::caf::{CafReader, CafWriter};
use hz_io::master::{Config, EventStream, MasterSet};
use std::fs;
use std::path::{Path, PathBuf};

pub struct Options {
    pub root: PathBuf,
    pub limit: Option<usize>,
    pub configs_only: bool,
    /// Also rewrite each audio component and compare it byte for byte.
    ///
    /// Off by default because a single component is gigabytes; on, it is the
    /// only check that exercises the container at full scale.
    pub audio: bool,
}

#[derive(Default)]
struct Tally {
    checked: usize,
    identical: usize,
    equivalent: usize,
    mismatched: usize,
    failed: usize,
}

impl Tally {
    fn report(&self, what: &str) {
        println!("\n{what}");
        println!("  checked                    {:>6}", self.checked);
        println!("  byte-identical             {:>6}", self.identical);
        println!("  equivalent, laid out anew  {:>6}", self.equivalent);
        println!("  MISMATCHED                 {:>6}", self.mismatched);
        println!("  unreadable                 {:>6}", self.failed);
    }
}

pub fn run(options: Options) -> Result<()> {
    let survey = super::corpus::survey(&options.root, options.limit, false)?;

    let mut configs = Tally::default();
    let mut streams = Tally::default();
    let mut audio = Tally::default();
    let mut complaints = Vec::new();

    // Rewritten components land under `target/`, never the system temp
    // directory: they are gigabytes, and tmpfs is memory.
    let work = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/roundtrip");

    let total = survey.masters.len();
    for (n, master) in survey.masters.iter().enumerate() {
        if n % 25 == 0 && n > 0 {
            println!("  … {n}/{total}");
        }

        check_config(&master.config, &mut configs, &mut complaints);

        if options.configs_only {
            continue;
        }
        let Ok(set) = MasterSet::open(&master.config) else {
            continue;
        };
        for components in &set.components {
            if let Some(path) = components.metadata.path() {
                check_stream(path, &mut streams, &mut complaints);
            }
            if options.audio
                && let Some(path) = components.audio.path()
            {
                check_audio(path, &work, &mut audio, &mut complaints);
            }
        }
    }

    configs.report("config documents");
    if !options.configs_only {
        streams.report("event streams");
        if options.audio {
            audio.report("audio components");
        }
    }

    if complaints.is_empty() {
        println!("\nno mismatches");
    } else {
        println!("\nmismatches ({} shown)", complaints.len().min(10));
        for complaint in complaints.iter().take(10) {
            println!("  {complaint}");
        }
    }

    Ok(())
}

fn check_config(path: &Path, tally: &mut Tally, complaints: &mut Vec<String>) {
    tally.checked += 1;
    let Ok(original) = fs::read_to_string(path) else {
        tally.failed += 1;
        return;
    };
    let parsed = match Config::parse(&original) {
        Ok(parsed) => parsed,
        Err(e) => {
            tally.failed += 1;
            complaints.push(format!("{}: {e}", path.display()));
            return;
        }
    };

    let written = parsed.to_yaml();
    match Config::parse(&written) {
        Ok(reparsed) if reparsed == parsed => {
            if written == original {
                tally.identical += 1;
            } else {
                tally.equivalent += 1;
            }
        }
        Ok(_) => {
            tally.mismatched += 1;
            complaints.push(describe_difference(path, &original, &written));
        }
        Err(e) => {
            tally.mismatched += 1;
            complaints.push(format!(
                "{}: rewritten form is unreadable: {e}",
                path.display()
            ));
        }
    }
}

fn check_stream(path: &Path, tally: &mut Tally, complaints: &mut Vec<String>) {
    tally.checked += 1;
    let Ok(original) = fs::read_to_string(path) else {
        tally.failed += 1;
        return;
    };
    let parsed = match EventStream::parse(&original) {
        Ok(parsed) => parsed,
        Err(e) => {
            tally.failed += 1;
            complaints.push(format!("{}: {e}", path.display()));
            return;
        }
    };

    let written = parsed.to_yaml();
    match EventStream::parse(&written) {
        Ok(reparsed) if reparsed == parsed => {
            if written == original {
                tally.identical += 1;
            } else {
                tally.equivalent += 1;
            }
        }
        Ok(_) => {
            tally.mismatched += 1;
            complaints.push(describe_difference(path, &original, &written));
        }
        Err(e) => {
            tally.mismatched += 1;
            complaints.push(format!(
                "{}: rewritten form is unreadable: {e}",
                path.display()
            ));
        }
    }
}

/// Rewrite one audio component and compare it with the original.
///
/// The comparison is byte for byte on purpose. A container that round-trips
/// "close enough" is a container that has silently changed the samples the
/// lossless track exists to preserve.
fn check_audio(path: &Path, work: &Path, tally: &mut Tally, complaints: &mut Vec<String>) {
    tally.checked += 1;
    match rewrite_audio(path, work) {
        Ok(copy) => {
            match same_bytes(path, &copy) {
                Ok(true) => tally.identical += 1,
                Ok(false) => {
                    tally.mismatched += 1;
                    complaints.push(format!("{}: rewritten component differs", path.display()));
                }
                Err(e) => {
                    tally.failed += 1;
                    complaints.push(e.to_string());
                }
            }
            let _ = fs::remove_file(&copy);
        }
        Err(e) => {
            tally.failed += 1;
            complaints.push(e.to_string());
        }
    }
}

fn rewrite_audio(path: &Path, work: &Path) -> Result<PathBuf> {
    /// Frames per block. Large enough that the per-call overhead vanishes,
    /// small enough that the buffer stays in cache-friendly territory.
    const BLOCK_FRAMES: usize = 8192;

    fs::create_dir_all(work).map_err(|e| Error::io(work, e))?;
    let copy = work.join(path.file_name().unwrap_or_default());

    let mut reader = CafReader::open(path)?;
    let format = *reader.format();
    let extra = reader.extra_chunks().to_vec();
    let mut writer = CafWriter::create_with_chunks(&copy, format, &extra)?;

    let mut block = vec![0i32; BLOCK_FRAMES * format.channels as usize];
    loop {
        let frames = reader.read_frames(&mut block)?;
        if frames == 0 {
            break;
        }
        writer.write_frames(&block[..frames * format.channels as usize])?;
    }
    writer.finish()?;
    Ok(copy)
}

fn same_bytes(a: &Path, b: &Path) -> Result<bool> {
    use std::io::Read;

    let mut a_file = fs::File::open(a).map_err(|e| Error::io(a, e))?;
    let mut b_file = fs::File::open(b).map_err(|e| Error::io(b, e))?;
    if a_file.metadata().map_err(|e| Error::io(a, e))?.len()
        != b_file.metadata().map_err(|e| Error::io(b, e))?.len()
    {
        return Ok(false);
    }

    let mut left = vec![0u8; 1 << 20];
    let mut right = vec![0u8; 1 << 20];
    loop {
        let n = read_some(&mut a_file, &mut left).map_err(|e| Error::io(a, e))?;
        if n == 0 {
            return Ok(true);
        }
        let m = read_some(&mut b_file, &mut right[..n]).map_err(|e| Error::io(b, e))?;
        if n != m || left[..n] != right[..n] {
            return Ok(false);
        }
    }

    fn read_some(file: &mut fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut filled = 0;
        while filled < buf.len() {
            match file.read(&mut buf[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        Ok(filled)
    }
}

/// Name the first line that differs. A mismatch with no location is a bug
/// report nobody can act on.
fn describe_difference(path: &Path, original: &str, written: &str) -> String {
    for (n, (a, b)) in original.lines().zip(written.lines()).enumerate() {
        if a != b {
            return format!("{}:{}: read `{a}`, wrote `{b}`", path.display(), n + 1);
        }
    }
    format!(
        "{}: identical for {} lines, then one side ends",
        path.display(),
        original.lines().count().min(written.lines().count())
    )
}
