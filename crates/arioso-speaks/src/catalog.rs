//! The catalog as the app shows it: voices of one engine, with what they cost and what
//! their licence allows.

use arioso_server::ModelView;

/// The engines this app offers, in the order the tabs appear.
pub const ENGINES: [&str; 2] = ["kokoro", "piper"];

/// One voice, as a row in the list.
#[derive(Debug, Clone)]
pub struct Voice {
    /// Catalog id, which is also what a download is asked for by.
    pub id: String,
    /// Engine that speaks it.
    pub engine: String,
    /// Whole model on disk, in bytes.
    pub bytes: u64,
    /// SPDX licence as the manifest states it.
    pub license: String,
    /// Whether the licence allows commercial use.
    pub commercial: bool,
    /// Speaker names, in the order the voice numbers them. Empty when there is one.
    pub speakers: Vec<String>,
    /// Languages the voice speaks.
    pub languages: Vec<String>,
    /// Whether every file is on disk.
    pub installed: bool,
}

impl Voice {
    /// Whether the licence needs a reader's attention before this voice ships.
    ///
    /// An unknown licence is not a licence, and the manifest's own `commercial_use` is the
    /// answer to the question a shipper actually asks.
    #[must_use]
    pub fn licence_needs_a_look(&self) -> bool {
        self.license.eq_ignore_ascii_case("unknown") || !self.commercial
    }

    /// The licence, with the reason it needs a look when it does.
    #[must_use]
    pub fn licence_label(&self) -> String {
        if self.license.eq_ignore_ascii_case("unknown") {
            "unknown licence".to_owned()
        } else if !self.commercial {
            format!("{} · not for commercial use", self.license)
        } else {
            self.license.clone()
        }
    }
}

/// The voices of one engine, in the order the catalog lists them.
#[must_use]
pub fn voices(models: &[ModelView], engine: &str) -> Vec<Voice> {
    models
        .iter()
        .filter(|model| model.kind == "tts" && model.engine == engine)
        .map(|model| Voice {
            id: model.id.clone(),
            engine: model.engine.clone(),
            bytes: model.bytes,
            license: model.license.clone(),
            commercial: model.commercial_use,
            speakers: model.speakers.clone(),
            languages: model.languages.clone(),
            installed: model.installed,
        })
        .collect()
}

/// The positions in `voices` whose id or language contains `filter`, case insensitively.
///
/// An empty filter matches everything, which is the state the list starts in.
#[must_use]
pub fn matching(voices: &[Voice], filter: &str) -> Vec<usize> {
    let needle = filter.trim().to_lowercase();
    if needle.is_empty() {
        return (0..voices.len()).collect();
    }
    voices
        .iter()
        .enumerate()
        .filter(|(_, voice)| {
            voice.id.to_lowercase().contains(&needle)
                || voice
                    .languages
                    .iter()
                    .any(|language| language.to_lowercase().contains(&needle))
        })
        .map(|(index, _)| index)
        .collect()
}

/// Whether a catalog id is the voice a config names.
///
/// A config may name the bare voice (`en_US-joe-medium`) or the whole id, because the
/// catalog id carries the engine in front of it.
#[must_use]
pub fn is_named(id: &str, name: &str) -> bool {
    id == name || id.ends_with(&format!("-{name}"))
}

/// A size a person can read at a glance.
#[must_use]
pub fn size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let value = bytes as f64;
    if value < KB {
        return format!("{bytes} B");
    }
    if value < KB * KB {
        return format!("{:.0} KB", value / KB);
    }
    format!("{:.0} MB", value / (KB * KB))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voice(id: &str, license: &str, commercial: bool) -> Voice {
        Voice {
            id: id.to_owned(),
            engine: "piper".to_owned(),
            bytes: 1024 * 1024,
            license: license.to_owned(),
            commercial,
            speakers: Vec::new(),
            languages: vec!["en-US".to_owned()],
            installed: false,
        }
    }

    #[test]
    fn an_unknown_licence_says_so_rather_than_showing_the_word() {
        let voice = voice("toy", "unknown", false);
        assert!(voice.licence_needs_a_look());
        assert_eq!(voice.licence_label(), "unknown licence");
    }

    #[test]
    fn a_licence_that_forbids_selling_says_that() {
        let voice = voice("toy", "CC-BY-NC-4.0", false);
        assert!(voice.licence_needs_a_look());
        assert!(voice.licence_label().contains("not for commercial use"));
    }

    #[test]
    fn a_plain_licence_is_shown_as_it_is() {
        let voice = voice("toy", "Apache-2.0", true);
        assert!(!voice.licence_needs_a_look());
        assert_eq!(voice.licence_label(), "Apache-2.0");
    }

    #[test]
    fn a_filter_matches_the_id_and_the_language() {
        let voices = vec![
            voice("af_heart", "MIT", true),
            voice("en_US-joe", "MIT", true),
        ];
        assert_eq!(matching(&voices, "af_").len(), 1);
        assert_eq!(matching(&voices, "en-us").len(), 2);
        assert_eq!(matching(&voices, "").len(), 2);
    }

    #[test]
    fn a_bare_voice_name_matches_the_catalog_id() {
        assert!(is_named("piper-en_US-joe-medium", "en_US-joe-medium"));
        assert!(is_named("piper-en_US-joe-medium", "piper-en_US-joe-medium"));
        assert!(!is_named("piper-en_US-joe-low", "en_US-joe-medium"));
    }

    #[test]
    fn sizes_are_read_at_a_glance() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(523_264), "511 KB");
        assert_eq!(size(353_749_136), "337 MB");
    }
}
