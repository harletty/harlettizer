//! The YAML shapes the master format actually uses, and a writer for them.
//!
//! Reading goes through `serde_yaml_ng`. Writing does not, and that is a
//! deliberate choice: the format writes numbers the way Rust's `Display`
//! writes them (`0`, not `0.0`), puts positions in flow sequences, quotes
//! nothing it does not have to, and carries `-inf` as a gain. Getting there
//! through a general-purpose emitter means serialising and then rewriting the
//! whole document as text to undo what the emitter did — which is what the
//! decoder's writer does, and it costs a second full pass over a document that
//! can hold tens of thousands of events. Writing the layout directly is both
//! exact and cheaper.

use serde::Deserialize;
use serde::de::{self, Deserializer, Unexpected, Visitor};
use std::fmt::{self, Display, Write};

/// A number as the master format writes it.
///
/// The format is not consistent about scalar types and cannot be made to be:
/// a position component is `-1` where a size is `0.0`, and a gain of silence
/// is the bare token `-inf`, which is a string to any YAML 1.2 parser and a
/// float to a human. One lenient type reading all of those, and `Display`
/// writing them back in the same spelling, is simpler than teaching every
/// field its own quirk.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Num(pub f64);

impl Display for Num {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `{}` on f64 gives `0`, `-1`, `-inf` and the shortest round-trip form
        // of everything else — exactly the spellings found in real masters.
        Display::fmt(&self.0, f)
    }
}

impl From<f64> for Num {
    fn from(v: f64) -> Self {
        Self(v)
    }
}

impl<'de> Deserialize<'de> for Num {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;

        impl Visitor<'_> for V {
            type Value = Num;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a number, or `-inf`")
            }

            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Num, E> {
                Ok(Num(v))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Num, E> {
                Ok(Num(v as f64))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Num, E> {
                Ok(Num(v as f64))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Num, E> {
                v.parse::<f64>()
                    .map(Num)
                    .map_err(|_| E::invalid_value(Unexpected::Str(v), &self))
            }
        }

        deserializer.deserialize_any(V)
    }
}

/// A number the format writes with an explicit fraction: `0.0`, not `0`.
///
/// The distinction from [`Num`] is inherited, not intrinsic — both spellings
/// parse to the same value, and it exists because the tooling that produced
/// every master in circulation formats a position one way and a size the
/// other. Matching it costs one type and makes our output indistinguishable
/// from everyone else's; not matching it would make every file we write look
/// subtly foreign for no gain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Real(pub f64);

impl Display for Real {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `{:?}` on f64 keeps the decimal point that `{}` drops.
        write!(f, "{:?}", self.0)
    }
}

impl From<f64> for Real {
    fn from(v: f64) -> Self {
        Self(v)
    }
}

impl<'de> Deserialize<'de> for Real {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Num::deserialize(deserializer).map(|n| Real(n.0))
    }
}

/// Deserialize a unit-only enum from a scalar that may not be a string.
///
/// `fps: 24` is a YAML number and `fps: 23.976` is a float, but both name a
/// frame rate whose canonical spelling is a string. Rendering the scalar back
/// to text before matching is the only way to accept the files that exist.
pub(crate) fn scalar_text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    struct V;

    impl Visitor<'_> for V {
        type Value = String;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a scalar")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<String, E> {
            Ok(v.to_string())
        }

        fn visit_f64<E: de::Error>(self, v: f64) -> Result<String, E> {
            Ok(v.to_string())
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<String, E> {
            Ok(v.to_string())
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<String, E> {
            Ok(v.to_string())
        }

        fn visit_bool<E: de::Error>(self, v: bool) -> Result<String, E> {
            Ok(v.to_string())
        }
    }

    deserializer.deserialize_any(V)
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Indentation step, and the width the format gives a sequence dash.
const STEP: usize = 2;

/// A writer for the subset of YAML the master format uses: nested maps,
/// block sequences, flow sequences of numbers, and plain scalars.
#[derive(Default)]
pub(crate) struct Writer {
    out: String,
    /// A dash has been written and owns the indentation of the next line.
    pending_dash: bool,
}

impl Writer {
    pub fn with_capacity(bytes: usize) -> Self {
        Self {
            out: String::with_capacity(bytes),
            pending_dash: false,
        }
    }

    pub fn finish(self) -> String {
        self.out
    }

    fn lead(&mut self, indent: usize) {
        if self.pending_dash {
            self.pending_dash = false;
            return;
        }
        for _ in 0..indent {
            self.out.push(' ');
        }
    }

    /// Open a sequence entry. The next `pair` or `key` lands on this line.
    pub fn dash(&mut self, indent: usize) {
        self.lead(indent);
        self.out.push_str("- ");
        self.pending_dash = true;
    }

    /// The indentation a sequence entry's fields sit at.
    pub const fn item_indent(parent: usize) -> usize {
        parent + STEP * 2
    }

    /// The indentation a sequence entry's dash sits at.
    pub const fn dash_indent(parent: usize) -> usize {
        parent + STEP
    }

    /// `key: value`
    pub fn pair(&mut self, indent: usize, key: &str, value: impl Display) {
        self.lead(indent);
        let _ = write!(self.out, "{key}: {value}");
        self.out.push('\n');
    }

    /// `key: value`, skipped entirely when there is no value.
    pub fn pair_opt(&mut self, indent: usize, key: &str, value: Option<impl Display>) {
        if let Some(value) = value {
            self.pair(indent, key, value);
        }
    }

    /// `key: <scalar>`, quoted only if it would not survive a round trip bare.
    pub fn text(&mut self, indent: usize, key: &str, value: &str) {
        self.lead(indent);
        let _ = write!(self.out, "{key}: ");
        self.push_scalar(value);
        self.out.push('\n');
    }

    pub fn text_opt(&mut self, indent: usize, key: &str, value: Option<&str>) {
        if let Some(value) = value {
            self.text(indent, key, value);
        }
    }

    /// `key:`, opening a nested map or sequence.
    pub fn key(&mut self, indent: usize, key: &str) {
        self.lead(indent);
        let _ = write!(self.out, "{key}:");
        self.out.push('\n');
    }

    /// `key: {}` — an explicitly empty map, which the format does emit.
    pub fn empty_map(&mut self, indent: usize, key: &str) {
        self.pair(indent, key, "{}");
    }

    /// `key: [a, b, c]`
    pub fn flow<T: Display>(&mut self, indent: usize, key: &str, values: &[T]) {
        self.lead(indent);
        let _ = write!(self.out, "{key}: [");
        for (i, v) in values.iter().enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            let _ = write!(self.out, "{v}");
        }
        self.out.push_str("]\n");
    }

    /// Write a string scalar, quoting only when leaving it bare would change
    /// what it means. Master sets name files, and those names really do
    /// contain brackets, ampersands, quotes and non-ASCII — the decoder's
    /// writer once corrupted exactly these by rewriting scalar text, so this
    /// one leaves the content alone and adds quotes when it must.
    fn push_scalar(&mut self, value: &str) {
        if needs_quoting(value) {
            self.out.push('\'');
            for c in value.chars() {
                if c == '\'' {
                    self.out.push('\'');
                }
                self.out.push(c);
            }
            self.out.push('\'');
        } else {
            self.out.push_str(value);
        }
    }
}

/// Whether a plain scalar would be misread if written bare.
fn needs_quoting(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    if value.trim() != value {
        return true;
    }
    // An indicator in first position starts a collection, an anchor, a tag…
    if value.starts_with([
        '-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@',
        '`',
    ]) {
        return true;
    }
    // `: ` ends a key anywhere; ` #` starts a comment.
    if value.contains(": ") || value.contains(" #") || value.ends_with(':') {
        return true;
    }
    // A string that would otherwise read back as something other than a
    // string. The YAML 1.1 booleans — `yes`, `no`, `on`, `off` — are
    // deliberately absent: 1.2 reads them as strings, the format's own tooling
    // writes `binauralRenderMode: off` bare, and quoting it here would make
    // every file we write look foreign for no gain. A 1.1 parser will read it
    // as `false`, but that is true of every master already in circulation.
    matches!(value, "true" | "false" | "null" | "~") || value.parse::<f64>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_written_the_way_masters_write_them() {
        assert_eq!(Num(0.0).to_string(), "0");
        assert_eq!(Num(-1.0).to_string(), "-1");
        assert_eq!(Num(f64::NEG_INFINITY).to_string(), "-inf");
        assert_eq!(Num(0.8666666666666667).to_string(), "0.8666666666666667");
    }

    #[test]
    fn sizes_keep_the_decimal_point_positions_drop() {
        assert_eq!(Real(0.0).to_string(), "0.0");
        assert_eq!(Real(1.0).to_string(), "1.0");
        assert_eq!(Real(0.25).to_string(), "0.25");
        assert_eq!(Num(0.0).to_string(), "0");
    }

    #[test]
    fn numbers_are_read_from_every_spelling_that_occurs() {
        let n: Vec<Num> = serde_yaml_ng::from_str("[0, -1, 0.25, -inf]").unwrap();
        assert_eq!(n[0], Num(0.0));
        assert_eq!(n[1], Num(-1.0));
        assert_eq!(n[2], Num(0.25));
        assert!(n[3].0.is_infinite() && n[3].0.is_sign_negative());
    }

    #[test]
    fn a_filename_is_never_mangled() {
        let mut w = Writer::default();
        w.text(0, "audio", "Astérix & Obélix [2002].1.atmos.audio");
        w.text(0, "metadata", "-leading-dash.metadata");
        let out = w.finish();
        assert!(out.contains("audio: Astérix & Obélix [2002].1.atmos.audio"));
        assert!(out.contains("metadata: '-leading-dash.metadata'"));
    }

    #[test]
    fn a_dash_owns_the_line_it_opens() {
        let mut w = Writer::default();
        w.key(0, "objects");
        w.dash(Writer::dash_indent(0));
        w.pair(Writer::item_indent(0), "ID", 10);
        w.pair(Writer::item_indent(0), "description", "x");
        assert_eq!(w.finish(), "objects:\n  - ID: 10\n    description: x\n");
    }
}
