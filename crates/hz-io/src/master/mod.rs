//! The master file set: a config, an event stream, and an audio component.

mod config;
mod metadata;

pub use config::{
    BedInstance, Channel, Config, DownmixMode, Fps, Object, Presentation, PresentationType,
    SourceCodec, TrimMode, TrimOptions, WarpMode,
};
pub use metadata::{Event, EventStream, Zones, screen_codes};

use hz_core::{Error, Result};

/// The master-format version this crate writes.
///
/// Every master seen carries this, and a set written with a version nothing
/// recognises is a set nothing will open.
pub const DAMF_VERSION: &str = "0.5.1";
use std::path::{Path, PathBuf};

/// A master set on disk: the config, plus where its components actually are.
#[derive(Debug, Clone)]
pub struct MasterSet {
    pub config_path: PathBuf,
    pub config: Config,
    /// One entry per presentation, in the config's order.
    pub components: Vec<Components>,
}

/// Where one presentation's components resolved to.
#[derive(Debug, Clone)]
pub struct Components {
    pub metadata: Component,
    pub audio: Component,
}

/// The outcome of resolving one component reference.
///
/// A third of the masters seen name components that are not what is on disk
/// — a stale name left by an earlier rescan. Resolving by convention rescues
/// them, and keeping the distinction in the type means a caller can report it
/// instead of discovering it later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Component {
    /// The config's name exists.
    Exact(PathBuf),
    /// The config's name is stale; the file named after the config exists.
    ByConvention { referenced: String, path: PathBuf },
    /// Neither name exists.
    Missing { referenced: String },
}

impl Component {
    /// The file to actually read, if there is one.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Exact(path) | Self::ByConvention { path, .. } => Some(path),
            Self::Missing { .. } => None,
        }
    }

    pub fn is_present(&self) -> bool {
        self.path().is_some()
    }

    /// Whether the config's own name for this component was wrong.
    pub fn is_stale(&self) -> bool {
        matches!(self, Self::ByConvention { .. })
    }

    fn resolve(dir: &Path, stem: &str, suffix: &str, referenced: &str) -> Self {
        let named = dir.join(referenced);
        if named.is_file() {
            return Self::Exact(named);
        }
        let conventional = dir.join(format!("{stem}.{suffix}"));
        if conventional.is_file() {
            return Self::ByConvention {
                referenced: referenced.to_string(),
                path: conventional,
            };
        }
        Self::Missing {
            referenced: referenced.to_string(),
        }
    }
}

impl MasterSet {
    /// Read the config at `path` and locate the components it names.
    pub fn open(path: &Path) -> Result<Self> {
        let config = Config::read(path)?;
        let dir = path.parent().unwrap_or(Path::new("."));
        let stem = stem_of(path);

        let components = config
            .presentations
            .iter()
            .map(|p| Components {
                metadata: Component::resolve(dir, stem, "atmos.metadata", &p.metadata),
                audio: Component::resolve(dir, stem, "atmos.audio", &p.audio),
            })
            .collect();

        Ok(Self {
            config_path: path.to_path_buf(),
            config,
            components,
        })
    }

    /// Read one presentation's event stream.
    pub fn read_events(&self, presentation: usize) -> Result<EventStream> {
        let components = self.components.get(presentation).ok_or_else(|| {
            Error::malformed(&self.config_path, format!("no presentation {presentation}"))
        })?;

        match components.metadata.path() {
            Some(path) => EventStream::read(path),
            None => Err(Error::MissingComponent {
                referenced_by: self.config_path.clone(),
                reference: self.config.presentations[presentation].metadata.clone(),
            }),
        }
    }
}

/// The stem every component of a set shares: `foo.1.atmos` → `foo.1`.
fn stem_of(config_path: &Path) -> &str {
    let name = config_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    name.strip_suffix(".atmos").unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Fixtures go under `target/`, never the system temp directory.
    fn fixture_dir(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-fixtures/hz-io")
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config_text(metadata: &str, audio: &str) -> String {
        format!(
            "version: 0.5.1\n\
             presentations:\n\
             \x20 - type: home\n\
             \x20   simplified: false\n\
             \x20   metadata: {metadata}\n\
             \x20   audio: {audio}\n\
             \x20   offset: 0.0\n\
             \x20   bedInstances:\n\
             \x20     - channels:\n\
             \x20         - channel: LFE\n\
             \x20           ID: 3\n\
             \x20   objects:\n\
             \x20     - ID: 10\n"
        )
    }

    #[test]
    fn resolves_components_that_are_named_correctly() {
        let dir = fixture_dir("exact");
        let config = dir.join("t.1.atmos");
        fs::write(
            &config,
            config_text("t.1.atmos.metadata", "t.1.atmos.audio"),
        )
        .unwrap();
        fs::write(dir.join("t.1.atmos.metadata"), "events:\n  - ID: 10\n").unwrap();
        fs::write(dir.join("t.1.atmos.audio"), []).unwrap();

        let set = MasterSet::open(&config).unwrap();
        assert!(matches!(set.components[0].metadata, Component::Exact(_)));
        assert!(set.components[0].audio.is_present());
        assert_eq!(set.read_events(0).unwrap().events.len(), 1);
    }

    #[test]
    fn rescues_a_stale_reference_and_says_so() {
        let dir = fixture_dir("stale");
        let config = dir.join("t.1.atmos");
        fs::write(&config, config_text("t.atmos.metadata", "t.atmos.audio")).unwrap();
        fs::write(dir.join("t.1.atmos.metadata"), "events:\n  - ID: 10\n").unwrap();

        let set = MasterSet::open(&config).unwrap();
        assert!(set.components[0].metadata.is_stale());
        assert!(!set.components[0].audio.is_present());
        assert_eq!(set.read_events(0).unwrap().events.len(), 1);
    }

    #[test]
    fn a_missing_event_stream_names_what_was_referenced() {
        let dir = fixture_dir("missing");
        let config = dir.join("t.1.atmos");
        fs::write(&config, config_text("gone.metadata", "gone.audio")).unwrap();

        let set = MasterSet::open(&config).unwrap();
        let err = set.read_events(0).unwrap_err();
        assert!(err.to_string().contains("gone.metadata"), "{err}");
    }
}
