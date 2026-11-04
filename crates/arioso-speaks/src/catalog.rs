//! The catalog as the app shows it: voices of one engine, with what they cost and what
//! their licence allows.

use arioso_server::ModelView;

/// The engines this app offers, in the order the tabs appear.
pub const ENGINES: [&str; 2] = ["kokoro", "piper"];

/// One voice, as a row in the list.
///
/// A Kokoro model is one file holding fifty-four styles, and the styles are what a person
/// chooses between. So a row is a style, and `model` is the file it comes from: the id a
/// download is asked for by is the model, never the style.
#[derive(Debug, Clone)]
pub struct Voice {
    /// What this row is called: a Piper catalog id, or a Kokoro style name.
    pub id: String,
    /// The catalog id the files belong to.
    pub model: String,
    /// Which style of that model, for an engine whose styles are numbered. Zero otherwise.
    pub sid: i64,
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

    /// The voice name without the engine in front of it.
    ///
    /// The list is already one engine's, so repeating it on every row spends six columns on
    /// something the tab above has said once.
    #[must_use]
    pub fn display_name(&self) -> String {
        self.id
            .strip_prefix(&format!("{}-", self.engine))
            .unwrap_or(&self.id)
            .to_owned()
    }

    /// The licence in the few columns a list row can spare.
    ///
    /// A row is one line wide, and the full label is a sentence. The long form stays in the
    /// header and in the download offer, where there is room to say what it means.
    #[must_use]
    pub fn licence_chip(&self) -> String {
        if self.license.eq_ignore_ascii_case("unknown") {
            return "unknown".to_owned();
        }
        let short = match self.license.as_str() {
            "CC0-1.0" => "CC0",
            "CC-BY-4.0" => "CC-BY",
            "CC-BY-SA-4.0" => "CC-BY-SA",
            "MIT" => "MIT",
            other => other,
        };
        if self.commercial {
            short.to_owned()
        } else {
            // The marker travels with the licence: a row that says CC-BY and nothing else
            // would suggest a use the manifest refuses.
            format!("{short}!")
        }
    }

    /// The licence, spelled out.
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
    let mut rows = Vec::new();
    for model in models
        .iter()
        .filter(|model| model.kind == "tts" && model.engine == engine)
    {
        // A Kokoro style is a voice: one row each, so the list answers "which voices can I
        // hear" rather than "which files did I download". A Piper multi-speaker voice is one
        // voice with speakers, and stays one row with a speaker control.
        if model.engine == "kokoro" && model.speakers.len() > 1 {
            for style in &model.speakers {
                rows.push(Voice {
                    id: style.clone(),
                    model: model.id.clone(),
                    // A Kokoro style is one speaker. The style is chosen by name, so the
                    // speaker index is zero: the list once carried the style's position here,
                    // and the engine answered, correctly, that speaker 25 does not exist.
                    sid: 0,
                    engine: model.engine.clone(),
                    bytes: model.bytes,
                    license: model.license.clone(),
                    commercial: model.commercial_use,
                    // One row per style already: a speaker list on the row would repeat the
                    // list the rows are.
                    speakers: Vec::new(),
                    languages: model.languages.clone(),
                    installed: model.installed,
                });
            }
            continue;
        }
        rows.push(Voice {
            id: model.id.clone(),
            model: model.id.clone(),
            sid: 0,
            engine: model.engine.clone(),
            bytes: model.bytes,
            license: model.license.clone(),
            commercial: model.commercial_use,
            speakers: model.speakers.clone(),
            languages: model.languages.clone(),
            installed: model.installed,
        });
    }
    rows
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
            model: id.to_owned(),
            sid: 0,
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
    fn a_kokoro_model_becomes_one_row_per_style() {
        // The file is one; the voices are fifty-four. A list that shows the file leaves a
        // person with nothing to choose and one name to get wrong.
        let model = ModelView {
            id: "kokoro-82m".to_owned(),
            kind: "tts".to_owned(),
            engine: "kokoro".to_owned(),
            license: "Apache-2.0".to_owned(),
            installed: true,
            state: "installed".to_owned(),
            downloaded: None,
            bytes: 337,
            commercial_use: true,
            speakers: vec!["af_heart".to_owned(), "af_bella".to_owned()],
            languages: vec!["en-US".to_owned()],
        };
        let rows = voices(&[model], "kokoro");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].id, "af_bella");
        // Both rows point at the one file, and the style is the index into it.
        assert_eq!(rows[1].model, "kokoro-82m");
        // The style is the voice, and every Kokoro style has one speaker.
        assert_eq!(rows[1].sid, 0);
        assert_eq!(rows[0].sid, 0);
        assert!(rows[0].speakers.is_empty(), "the rows are the list");
    }

    #[test]
    fn a_chip_is_short_and_keeps_the_warning() {
        let mut voice = Voice {
            id: "piper-x".to_owned(),
            model: "piper-x".to_owned(),
            sid: 0,
            engine: "piper".to_owned(),
            bytes: 1,
            license: "CC-BY-4.0".to_owned(),
            commercial: false,
            speakers: Vec::new(),
            languages: Vec::new(),
            installed: false,
        };
        assert_eq!(voice.licence_chip(), "CC-BY!");
        voice.commercial = true;
        assert_eq!(voice.licence_chip(), "CC-BY");
        voice.license = "unknown".to_owned();
        assert_eq!(voice.licence_chip(), "unknown");
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
