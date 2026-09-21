//! Master-set survey.
//!
//! What matters about a directory of master sets is not how many there are
//! but what each one can actually prove:
//!
//! * a set with both metadata **and** audio supports the bit-exact round-trip
//!   check that is the whole point of the lossless track;
//! * a set with metadata only still supports the metadata round-trip, the ADM
//!   projection, and — most valuable of all — it is real object-trajectory
//!   data for the clustering work in `hz-cluster`;
//! * a set whose components cannot be resolved proves nothing, and saying so
//!   early is better than discovering it inside a test run.
//!
//! So the survey classifies rather than counts, and every later phase reports
//! through it.

use crate::mounts::MountTable;
use hz_core::{Error, Result};
use hz_io::master::{Component, MasterSet};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// How deep to look for master sets below the root.
///
/// The layout is one directory per set, so 2 is enough; the extra level
/// covers a set that sits in a subdirectory of its own.
const MAX_DEPTH: usize = 3;

pub struct Options {
    pub root: PathBuf,
    pub json: Option<PathBuf>,
    pub list: bool,
    pub limit: Option<usize>,
    pub sources: bool,
}

/// What a master set can be used to test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// Metadata and audio both present: everything, including PCM round-trip.
    Full,
    /// Metadata only: metadata round-trip, ADM projection, clustering input.
    MetadataOnly,
    /// Components missing or unreadable: nothing.
    Unusable,
}

impl Class {
    fn label(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::MetadataOnly => "metadata-only",
            Self::Unusable => "unusable",
        }
    }
}

/// Whether the source this master was decoded from can still be reached.
///
/// The audio component of a master set is large and is routinely pruned, so a
/// metadata-only master is not a dead end: if the source it was decoded from
/// is still reachable, the audio can be regenerated. That is the difference
/// between "this master cannot test PCM" and "this master cannot test PCM
/// today", which is a different sentence entirely.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// Recorded and present on this machine right now.
    Reachable { path: PathBuf },
    /// Recorded, but not reachable from here (moved file, or a volume whose
    /// automount trigger has not fired — which is not looked at, deliberately).
    Recorded { path: String },
    /// No source was recorded for this master.
    Unrecorded,
    /// Not looked for. Checking means stat'ing network paths, which is opt-in.
    NotChecked,
}

/// How one component reference resolved, in a shape the JSON report can hold.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Resolved {
    Exact { path: PathBuf },
    ByConvention { referenced: String, path: PathBuf },
    Missing { referenced: String },
}

impl Resolved {
    fn of(component: &Component) -> Self {
        match component {
            Component::Exact(path) => Self::Exact { path: path.clone() },
            Component::ByConvention { referenced, path } => Self::ByConvention {
                referenced: referenced.clone(),
                path: path.clone(),
            },
            Component::Missing { referenced } => Self::Missing {
                referenced: referenced.clone(),
            },
        }
    }

    fn is_present(&self) -> bool {
        !matches!(self, Self::Missing { .. })
    }
}

#[derive(Debug, Serialize)]
pub struct PresentationReport {
    pub kind: Option<String>,
    pub fps: Option<String>,
    pub offset: Option<f64>,
    pub creation_tool: Option<String>,
    pub source_codec: Option<String>,
    pub bed_channels: usize,
    pub objects: usize,
    pub metadata: Resolved,
    pub audio: Resolved,
}

#[derive(Debug, Serialize)]
pub struct MasterReport {
    pub title: String,
    pub config: PathBuf,
    pub version: Option<String>,
    pub language: Option<String>,
    pub presentations: Vec<PresentationReport>,
    pub class: Class,
    pub source: Source,
}

#[derive(Debug, Serialize)]
pub struct Survey {
    pub root: PathBuf,
    pub masters: Vec<MasterReport>,
    /// Configs that exist but could not be read. Reported, never silently skipped.
    pub failures: Vec<Failure>,
}

#[derive(Debug, Serialize)]
pub struct Failure {
    pub config: PathBuf,
    pub reason: String,
}

// ---------------------------------------------------------------------------
// The `.location` sidecar. Not part of the master format — it is how a set
// records where it was decoded from — so it is modelled here rather than in
// `hz-io`.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawLocation {
    #[serde(default)]
    local: Option<RawLocationEntry>,
    #[serde(default)]
    online: Option<RawLocationEntry>,
}

#[derive(Debug, Deserialize)]
struct RawLocationEntry {
    #[serde(default)]
    path: Option<String>,
}

// ---------------------------------------------------------------------------

pub fn run(options: Options) -> Result<()> {
    let root = options.root;

    if !root.is_dir() {
        return Err(Error::malformed(&root, "corpus root is not a directory"));
    }

    let survey = survey(&root, options.limit, options.sources)?;
    print_report(&survey, options.list);

    if let Some(path) = options.json {
        let json = serde_json::to_string_pretty(&survey)
            .map_err(|e| Error::malformed(&path, format!("cannot serialise report: {e}")))?;
        fs::write(&path, json).map_err(|e| Error::io(&path, e))?;
        println!("\nwrote {}", path.display());
    }

    Ok(())
}

pub fn survey(root: &Path, limit: Option<usize>, check_sources: bool) -> Result<Survey> {
    let mut configs = Vec::new();
    collect_configs(root, 0, &mut configs)?;
    configs.sort();
    if let Some(n) = limit {
        configs.truncate(n);
    }

    let mounts = MountTable::read();
    let mut masters = Vec::new();
    let mut failures = Vec::new();
    for config in configs {
        match read_master(&config, check_sources.then_some(&mounts)) {
            Ok(m) => masters.push(m),
            Err(e) => failures.push(Failure {
                config,
                reason: e.to_string(),
            }),
        }
    }

    Ok(Survey {
        root: root.to_path_buf(),
        masters,
        failures,
    })
}

fn collect_configs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) -> Result<()> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let entries = fs::read_dir(dir).map_err(|e| Error::io(dir, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::io(dir, e))?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|e| Error::io(&path, e))?;
        if file_type.is_dir() {
            // A symlinked mount that is not currently reachable must not abort
            // the whole survey: report nothing for it and carry on.
            let _ = collect_configs(&path, depth + 1, out);
        } else if path.extension().is_some_and(|e| e == "atmos") {
            out.push(path);
        }
    }
    Ok(())
}

fn read_master(config: &Path, mounts: Option<&MountTable>) -> Result<MasterReport> {
    let set = MasterSet::open(config)?;

    let presentations: Vec<PresentationReport> = set
        .config
        .presentations
        .iter()
        .zip(&set.components)
        .map(|(p, c)| PresentationReport {
            kind: Some(p.presentation_type.to_string()),
            fps: p.fps.map(|f| f.to_string()),
            offset: Some(p.offset.0),
            creation_tool: match (&p.creation_tool, &p.creation_tool_version) {
                (Some(t), Some(v)) => Some(format!("{t} {v}")),
                (Some(t), None) => Some(t.clone()),
                _ => None,
            },
            source_codec: p.source_codec.map(|c| c.to_string()),
            bed_channels: p.bed_instances.iter().map(|b| b.channels.len()).sum(),
            objects: p.objects.len(),
            metadata: Resolved::of(&c.metadata),
            audio: Resolved::of(&c.audio),
        })
        .collect();

    let class = classify(&presentations);
    let dir = config.parent().unwrap_or(Path::new("."));
    let title = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let source = match mounts {
        Some(mounts) => locate_source(dir, mounts),
        None => Source::NotChecked,
    };

    Ok(MasterReport {
        title,
        config: config.to_path_buf(),
        version: Some(set.config.version),
        language: set.config.language,
        presentations,
        class,
        source,
    })
}

/// Read the sidecar that records where a master was decoded from.
///
/// A recorded path is only stat'ed once the mount table says doing so cannot
/// trigger an automount; otherwise it is reported as recorded-but-unreachable
/// without being touched. The online path is never stat'ed at all.
fn locate_source(dir: &Path, mounts: &MountTable) -> Source {
    let Ok(entries) = fs::read_dir(dir) else {
        return Source::Unrecorded;
    };
    let sidecar = entries
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "location"));

    let Some(sidecar) = sidecar else {
        return Source::Unrecorded;
    };
    let Ok(text) = fs::read_to_string(&sidecar) else {
        return Source::Unrecorded;
    };
    let Ok(raw) = serde_yaml_ng::from_str::<RawLocation>(&text) else {
        return Source::Unrecorded;
    };

    if let Some(local) = raw.local.and_then(|l| l.path) {
        let path = PathBuf::from(&local);
        if !mounts.is_behind_trigger(&path) && path.is_file() {
            return Source::Reachable { path };
        }
        return Source::Recorded { path: local };
    }
    match raw.online.and_then(|o| o.path) {
        Some(path) => Source::Recorded { path },
        None => Source::Unrecorded,
    }
}

/// A master is worth what its best presentation is worth.
fn classify(presentations: &[PresentationReport]) -> Class {
    let mut best = Class::Unusable;
    for p in presentations {
        let class = match (p.metadata.is_present(), p.audio.is_present()) {
            (true, true) => Class::Full,
            (true, false) => Class::MetadataOnly,
            _ => Class::Unusable,
        };
        best = best.min(class);
    }
    best
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

fn print_report(survey: &Survey, list: bool) {
    println!("corpus: {}", survey.root.display());
    println!("masters: {}", survey.masters.len());

    let mut by_class: BTreeMap<Class, usize> = BTreeMap::new();
    let mut objects: BTreeMap<usize, usize> = BTreeMap::new();
    let mut beds: BTreeMap<usize, usize> = BTreeMap::new();
    let mut tools: BTreeMap<String, usize> = BTreeMap::new();
    let mut codecs: BTreeMap<String, usize> = BTreeMap::new();
    let mut fps: BTreeMap<String, usize> = BTreeMap::new();
    let mut stale_references = 0usize;
    let mut missing_audio = 0usize;

    for m in &survey.masters {
        *by_class.entry(m.class).or_default() += 1;
        for p in &m.presentations {
            *objects.entry(p.objects).or_default() += 1;
            *beds.entry(p.bed_channels).or_default() += 1;
            if let Some(t) = &p.creation_tool {
                *tools.entry(t.clone()).or_default() += 1;
            }
            *codecs
                .entry(
                    p.source_codec
                        .clone()
                        .unwrap_or_else(|| "(not recorded)".to_string()),
                )
                .or_default() += 1;
            if let Some(f) = &p.fps {
                *fps.entry(f.clone()).or_default() += 1;
            }
            for r in [&p.metadata, &p.audio] {
                if matches!(r, Resolved::ByConvention { .. }) {
                    stale_references += 1;
                }
            }
            if matches!(p.audio, Resolved::Missing { .. }) {
                missing_audio += 1;
            }
        }
    }

    println!("\nwhat this corpus can prove");
    for (class, count) in &by_class {
        let usable_for = match class {
            Class::Full => "PCM round-trip, metadata round-trip, differential runs",
            Class::MetadataOnly => "metadata round-trip, ADM projection, clustering input",
            Class::Unusable => "nothing",
        };
        println!("  {:<14} {:>5}   {usable_for}", class.label(), count);
    }

    if !survey.failures.is_empty() {
        println!(
            "  {:<14} {:>5}   unreadable configs",
            "failed",
            survey.failures.len()
        );
    }

    println!("\nobject count per presentation");
    print_histogram(&objects);

    println!("\nbed channels per presentation");
    print_histogram(&beds);

    if !fps.is_empty() {
        println!("\nframe rate");
        for (k, v) in &fps {
            println!("  {k:<14} {v:>5}");
        }
    }

    if !tools.is_empty() {
        println!("\ncreation tool");
        for (k, v) in &tools {
            println!("  {k:<24} {v:>5}");
        }
    }

    println!("\nsource codec");
    for (k, v) in &codecs {
        println!("  {k:<24} {v:>5}");
    }

    println!("\nreference resolution");
    println!("  stale, resolved by convention   {stale_references:>5}");
    println!("  audio referenced but absent     {missing_audio:>5}");

    let checked = survey
        .masters
        .iter()
        .any(|m| !matches!(m.source, Source::NotChecked));
    if checked {
        let mut reachable = 0usize;
        let mut recorded = 0usize;
        let mut unrecorded = 0usize;
        for m in &survey.masters {
            match m.source {
                Source::Reachable { .. } => reachable += 1,
                Source::Recorded { .. } => recorded += 1,
                Source::Unrecorded => unrecorded += 1,
                Source::NotChecked => {}
            }
        }
        println!("\nsources (audio can be regenerated from these)");
        println!("  reachable now                   {reachable:>5}");
        println!("  recorded, volume not mounted    {recorded:>5}");
        println!("  no source recorded              {unrecorded:>5}");
    } else {
        println!("\nsources: not checked (pass --sources)");
    }

    if list {
        println!("\nmasters");
        for m in &survey.masters {
            let p = m.presentations.first();
            println!(
                "  {:<14} {:>3} obj  {:>2} bed   {}",
                m.class.label(),
                p.map(|p| p.objects).unwrap_or(0),
                p.map(|p| p.bed_channels).unwrap_or(0),
                m.title,
            );
        }
    }

    for f in &survey.failures {
        eprintln!("failed: {}", f.reason);
    }
}

fn print_histogram(counts: &BTreeMap<usize, usize>) {
    let max = counts.values().copied().max().unwrap_or(1).max(1);
    for (value, count) in counts {
        let width = (count * 40).div_ceil(max);
        println!("  {value:>3}  {count:>5}  {}", "#".repeat(width));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixtures go under `target/`, never the system temp directory: this
    /// workspace keeps generated files out of tmpfs by house rule.
    fn fixture_dir(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target/test-fixtures")
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config_text(metadata: &str, audio: &str) -> String {
        format!(
            "language: eng\n\
             version: 0.5.1\n\
             presentations:\n\
             \x20 - type: home\n\
             \x20   simplified: false\n\
             \x20   metadata: {metadata}\n\
             \x20   audio: {audio}\n\
             \x20   offset: 0.0\n\
             \x20   fps: 24\n\
             \x20   creationTool: truehdd\n\
             \x20   creationToolVersion: 0.4.0\n\
             \x20   bedInstances:\n\
             \x20     - channels:\n\
             \x20         - channel: LFE\n\
             \x20           ID: 3\n\
             \x20   objects:\n\
             \x20     - ID: 10\n\
             \x20     - ID: 11\n"
        )
    }

    #[test]
    fn full_set_is_classified_full() {
        let dir = fixture_dir("full");
        fs::write(
            dir.join("t.1.atmos"),
            config_text("t.1.atmos.metadata", "t.1.atmos.audio"),
        )
        .unwrap();
        fs::write(dir.join("t.1.atmos.metadata"), "sampleRate: 48000\n").unwrap();
        fs::write(dir.join("t.1.atmos.audio"), []).unwrap();

        let survey = survey(&dir, None, false).unwrap();
        assert_eq!(survey.masters.len(), 1);
        let m = &survey.masters[0];
        assert_eq!(m.class, Class::Full);
        assert_eq!(m.presentations[0].objects, 2);
        assert_eq!(m.presentations[0].bed_channels, 1);
        assert_eq!(m.presentations[0].fps.as_deref(), Some("24"));
        assert_eq!(
            m.presentations[0].creation_tool.as_deref(),
            Some("truehdd 0.4.0")
        );
    }

    #[test]
    fn audio_absent_is_metadata_only_not_unusable() {
        let dir = fixture_dir("metaonly");
        fs::write(
            dir.join("t.1.atmos"),
            config_text("t.1.atmos.metadata", "t.1.atmos.audio"),
        )
        .unwrap();
        fs::write(dir.join("t.1.atmos.metadata"), "sampleRate: 48000\n").unwrap();

        let survey = survey(&dir, None, false).unwrap();
        assert_eq!(survey.masters[0].class, Class::MetadataOnly);
    }

    /// Real sets exist whose config references do not match the files on
    /// disk. Those masters are usable and must not be thrown away.
    #[test]
    fn stale_reference_resolves_by_convention() {
        let dir = fixture_dir("stale");
        fs::write(
            dir.join("t.1.atmos"),
            config_text("t.atmos.metadata", "t.atmos.audio"),
        )
        .unwrap();
        fs::write(dir.join("t.1.atmos.metadata"), "sampleRate: 48000\n").unwrap();

        let survey = survey(&dir, None, false).unwrap();
        let m = &survey.masters[0];
        assert_eq!(m.class, Class::MetadataOnly);
        assert!(matches!(
            m.presentations[0].metadata,
            Resolved::ByConvention { .. }
        ));
        assert!(matches!(m.presentations[0].audio, Resolved::Missing { .. }));
    }

    #[test]
    fn unreadable_config_is_reported_not_skipped() {
        let dir = fixture_dir("broken");
        fs::write(dir.join("t.1.atmos"), "this: [is not: a config\n").unwrap();

        let survey = survey(&dir, None, false).unwrap();
        assert!(survey.masters.is_empty());
        assert_eq!(survey.failures.len(), 1);
    }

    #[test]
    fn missing_root_is_an_error() {
        let missing = fixture_dir("gone").join("nope");
        let err = run(Options {
            root: missing,
            json: None,
            list: false,
            limit: None,
            sources: false,
        })
        .unwrap_err();
        assert!(err.to_string().contains("not a directory"));
    }
}
