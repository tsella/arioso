//! Supertonic voice inference.
//!
//! Four graphs, one voice style, characters in and 44.1 kHz audio out: a text encoder, a
//! duration predictor, a flow-matching vector field, and a vocoder. There is no phoneme
//! dictionary in this path, because the model was trained on letters — the vocabulary is a
//! table of code points, and the front end is [`text::preprocess`].
//!
//! # What this engine does not do
//!
//! It ignores the pronunciation dictionary the other engines use. A spoken form written as
//! phonemes is not a sequence of characters, and feeding it here would speak the phoneme
//! names. The report says which words the dictionary would have corrected is therefore empty
//! rather than wrong.

mod noise;
pub mod text;

use std::path::{Path, PathBuf};

use arioso_core::UserDictionary;
use arioso_g2p::CmuDict;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::{Tensor, TensorRef};

/// Flow-matching steps per clause.
///
/// The SDK default is 5 and the reference implementation uses 8. Eight is the number this
/// crate ships because the difference is audible at 5 on long clauses and costs one graph
/// evaluation each; a caller that wants 5 has the constant to change, and the measurement to
/// argue with.
pub const STEPS: usize = 8;

/// Longest clause handed to the model in one call, in characters.
pub const MAX_CHUNK: usize = 300;

/// The seed every render starts from.
///
/// Fixed, so the same text at the same speed gives the same audio. The clause index is added,
/// so two clauses of one document do not share their starting noise.
const SEED: u64 = 0x6172_696F_736F_0001;

/// The files one voice needs.
#[derive(Debug, Clone)]
pub struct Files<'a> {
    /// Text encoder: characters and style to a text embedding.
    pub text_encoder: &'a Path,
    /// Duration predictor: characters and style to seconds.
    pub duration_predictor: &'a Path,
    /// Vector field: the flow-matching step.
    pub vector_estimator: &'a Path,
    /// Vocoder: latent frames to samples.
    pub vocoder: &'a Path,
    /// `tts.json`: rates, latent shape, chunk size.
    pub config: &'a Path,
    /// `unicode_indexer.json`: code point to id.
    pub indexer: &'a Path,
    /// The voice style, one JSON per voice.
    pub style: &'a Path,
}

/// How a render is asked for.
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    /// Speaking rate multiplier. Durations are divided by it.
    pub speed: f32,
}

/// What a render found in the text.
///
/// Both lists are always empty: this engine has no dictionary to miss a word from. The type is
/// the one the other engines report with, so the caller has one shape to handle.
#[derive(Debug, Default, Clone)]
pub struct RenderReport {
    /// Words spoken by a spelling rule rather than a dictionary entry.
    pub guessed: Vec<String>,
    /// Words nothing could pronounce.
    pub missing: Vec<String>,
    /// User dictionary entries the voice cannot say.
    pub unusable: Vec<String>,
}

/// What went wrong.
#[derive(Debug, thiserror::Error)]
pub enum SupertonicError {
    /// A file could not be read.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// Why it could not be read.
        source: std::io::Error,
    },
    /// A file was read and is not what it should be.
    #[error("{0}")]
    Config(String),
    /// ONNX Runtime refused a graph or a tensor.
    #[error("{0}")]
    Model(String),
    /// The caller's sink refused the audio.
    #[error("the audio consumer stopped")]
    Consumer,
}

impl From<ort::Error> for SupertonicError {
    fn from(error: ort::Error) -> Self {
        Self::Model(error.to_string())
    }
}

/// The shape of the model, read from `tts.json`.
#[derive(Debug, Clone, Copy)]
struct Shape {
    rate: u32,
    /// Samples per latent frame: `base_chunk_size * chunk_compress_factor`.
    frame: usize,
    /// Channels in the latent.
    channels: usize,
}

/// A loaded voice.
pub struct Voice {
    shape: Shape,
    lang: String,
    indexer: Vec<i32>,
    /// The style, flattened, as the graphs take it.
    style_ttl: Vec<f32>,
    style_dp: Vec<f32>,
    text_encoder: Session,
    duration_predictor: Session,
    vector_estimator: Session,
    vocoder: Session,
}

impl std::fmt::Debug for Voice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Voice")
            .field("rate", &self.shape.rate)
            .field("lang", &self.lang)
            .finish_non_exhaustive()
    }
}

impl Voice {
    /// Load the four graphs, the vocabulary and one voice style.
    ///
    /// `lang` is the tag the text is wrapped in; the model is multilingual and the tag is how
    /// it knows which one to speak.
    ///
    /// # Errors
    ///
    /// A file that cannot be read or parsed, or a graph the runtime rejects.
    pub fn load_with_threads(
        files: &Files<'_>,
        lang: &str,
        threads: u32,
    ) -> Result<Self, SupertonicError> {
        if !text::is_language(lang) {
            return Err(SupertonicError::Config(format!(
                "{lang} is not a language this model speaks"
            )));
        }
        let shape = read_shape(files.config)?;
        let indexer = read_indexer(files.indexer)?;
        let (style_ttl, style_dp) = read_style(files.style)?;
        let threads = usize::try_from(threads).unwrap_or(1).max(1);
        let session = |path: &Path| -> Result<Session, SupertonicError> {
            Ok(Session::builder()?
                .with_optimization_level(GraphOptimizationLevel::Level3)?
                .with_intra_threads(threads)?
                .commit_from_file(path)?)
        };
        Ok(Self {
            shape,
            lang: lang.to_owned(),
            indexer,
            style_ttl,
            style_dp,
            text_encoder: session(files.text_encoder)?,
            duration_predictor: session(files.duration_predictor)?,
            vector_estimator: session(files.vector_estimator)?,
            vocoder: session(files.vocoder)?,
        })
    }

    /// The rate the model answers at.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.shape.rate
    }

    /// The language tag text is spoken as.
    #[must_use]
    pub fn language(&self) -> &str {
        &self.lang
    }

    /// Speak `text`, handing each clause's audio to `sink` as it is made.
    ///
    /// The dictionary arguments are accepted and ignored; see the crate note. Clauses come
    /// from [`arioso_core::clauses`], so the first audio leaves after the first comma rather
    /// than after the first full stop.
    ///
    /// # Errors
    ///
    /// A graph that fails, or a sink that refuses the audio.
    pub fn render(
        &mut self,
        text: &str,
        _dictionary: &CmuDict,
        _user: &UserDictionary,
        options: RenderOptions,
        mut sink: impl FnMut(&[f32], u32) -> Result<(), SupertonicError>,
    ) -> Result<RenderReport, SupertonicError> {
        let speed = if options.speed > 0.0 {
            options.speed
        } else {
            1.0
        };
        for (index, clause) in arioso_core::clauses(text).into_iter().enumerate() {
            for (part, chunk) in arioso_core::chunk_text(clause, MAX_CHUNK)
                .into_iter()
                .enumerate()
            {
                let seed = SEED.wrapping_add((index * 16 + part) as u64);
                self.infer(chunk, speed, seed, &mut sink)?;
            }
        }
        Ok(RenderReport::default())
    }

    /// One clause through the four graphs, its audio handed to `sink` as it is made.
    ///
    /// Nothing here copies a tensor it does not own. The style, the masks and the step counters
    /// are views on buffers that outlive the call, the latent is updated in place between the
    /// denoising steps, and the vocoder's output goes to `sink` as the borrowed slice it
    /// already is. The old shape copied the 51 KB style on every clause, the text embedding and
    /// the style again on all eight steps, and the audio twice at the end.
    fn infer<S>(
        &mut self,
        clause: &str,
        speed: f32,
        seed: u64,
        sink: &mut S,
    ) -> Result<(), SupertonicError>
    where
        S: FnMut(&[f32], u32) -> Result<(), SupertonicError>,
    {
        let processed = text::preprocess(clause, &self.lang);
        let ids = text::index(&processed, &self.indexer);
        if ids.is_empty() {
            return Ok(());
        }
        let length = ids.len();
        let mask = text::mask(length);

        let text_ids = Tensor::from_array(([1_i64, length as i64], ids))?;
        let text_mask = Tensor::from_array(([1_i64, 1, length as i64], mask.clone()))?;
        // The style is 51 KB and does not change between clauses, so each graph reads it in
        // place. A view costs one tensor header; the copy it replaces cost 51 KB of memory
        // traffic per clause, and again on every denoising step.
        let style_dp = TensorRef::from_array_view(([1_usize, 8, 16], &self.style_dp[..]))?;

        // How long the clause is, in seconds. The predictor is the only graph that knows, and
        // it answers before the encoder runs, so a clause that predicts nothing can stop here.
        let duration = self
            .duration_predictor
            .run(ort::inputs![
                "text_ids" => &text_ids,
                "style_dp" => style_dp,
                "text_mask" => &text_mask
            ])?
            .get("duration")
            .ok_or_else(|| SupertonicError::Model("the duration graph has no duration".into()))?
            .try_extract_tensor::<f32>()?
            .1
            .first()
            .copied()
            .unwrap_or(0.0);
        let seconds = f64::from(duration) / f64::from(speed);
        if !seconds.is_finite() || seconds <= 0.0 {
            return Ok(());
        }

        let style_ttl = TensorRef::from_array_view(([1_usize, 50, 256], &self.style_ttl[..]))?;
        let text_emb = self
            .text_encoder
            .run(ort::inputs![
                "text_ids" => &text_ids,
                "style_ttl" => style_ttl,
                "text_mask" => &text_mask
            ])?
            .get("text_emb")
            .ok_or_else(|| SupertonicError::Model("the encoder has no text_emb".into()))?
            .try_extract_tensor::<f32>()?
            .1
            .to_vec();

        let samples = (seconds * f64::from(self.shape.rate)).round() as usize;
        let frames = samples.div_ceil(self.shape.frame).max(1);
        let channels = self.shape.channels;

        // Two latent buffers, swapped: the step writes into the one that is not being read, so
        // no step allocates. The mask and the step total are the same on every step and are
        // built once.
        let mut latent = noise::normal(seed, frames * channels);
        let mut next = vec![0.0_f32; latent.len()];
        let latent_mask = vec![1.0_f32; frames];
        let total_step = [STEPS as f32];

        for step in 0..STEPS {
            let current_step = [step as f32];
            let outputs = {
                // The views live only as long as the call, so the block ends the borrow of
                // `latent` before it is swapped below.
                let noisy = TensorRef::from_array_view(([1_usize, channels, frames], &latent[..]))?;
                let emb = TensorRef::from_array_view(([1_usize, 256, length], &text_emb[..]))?;
                let latent_mask_view =
                    TensorRef::from_array_view(([1_usize, 1, frames], &latent_mask[..]))?;
                let current = TensorRef::from_array_view(([1_usize], &current_step[..]))?;
                let total = TensorRef::from_array_view(([1_usize], &total_step[..]))?;
                let style_ttl =
                    TensorRef::from_array_view(([1_usize, 50, 256], &self.style_ttl[..]))?;
                self.vector_estimator.run(ort::inputs![
                    "noisy_latent" => noisy,
                    "text_emb" => emb,
                    "style_ttl" => style_ttl,
                    "latent_mask" => latent_mask_view,
                    "text_mask" => &text_mask,
                    "current_step" => current,
                    "total_step" => total
                ])?
            };
            let denoised = outputs
                .get("denoised_latent")
                .ok_or_else(|| {
                    SupertonicError::Model("the vector field has no denoised_latent".into())
                })?
                .try_extract_tensor::<f32>()?
                .1;
            next.copy_from_slice(denoised);
            std::mem::swap(&mut latent, &mut next);
        }

        let final_latent = TensorRef::from_array_view(([1_usize, channels, frames], &latent[..]))?;
        let outputs = self.vocoder.run(ort::inputs!["latent" => final_latent])?;
        let audio = outputs
            .get("wav_tts")
            .ok_or_else(|| SupertonicError::Model("the vocoder has no wav_tts".into()))?
            .try_extract_tensor::<f32>()?
            .1;

        // The vocoder answers a whole number of latent frames; the predictor said how many
        // samples the clause is. Trimming is what keeps a clause from running into the next.
        let heard = audio.len().min(samples);
        if heard > 0 {
            sink(&audio[..heard], self.shape.rate)?;
        }
        Ok(())
    }
}

/// `tts.json` is a large document; three numbers are read from it.
fn read_shape(path: &Path) -> Result<Shape, SupertonicError> {
    let document = read_json(path)?;
    let number = |pointer: &str| -> Result<u64, SupertonicError> {
        document
            .pointer(pointer)
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                SupertonicError::Config(format!("{pointer} is missing from {}", path.display()))
            })
    };
    let rate = u32::try_from(number("/ae/sample_rate")?)
        .map_err(|_| SupertonicError::Config("ae.sample_rate is out of range".into()))?;
    let base = number("/ae/base_chunk_size")?;
    let compress = number("/ttl/chunk_compress_factor")?;
    let channels = number("/ttl/latent_dim")?;
    let frame = usize::try_from(base * compress)
        .map_err(|_| SupertonicError::Config("the frame size is out of range".into()))?;
    let channels = usize::try_from(channels * compress)
        .map_err(|_| SupertonicError::Config("the latent width is out of range".into()))?;
    Ok(Shape {
        rate,
        frame,
        channels,
    })
}

fn read_indexer(path: &Path) -> Result<Vec<i32>, SupertonicError> {
    let document = read_json(path)?;
    let table = document
        .as_array()
        .ok_or_else(|| SupertonicError::Config(format!("{} is not an array", path.display())))?;
    Ok(table
        .iter()
        .map(|value| {
            value
                .as_i64()
                .and_then(|id| i32::try_from(id).ok())
                .unwrap_or(-1)
        })
        .collect())
}

/// The style file holds two flattened tensors and some provenance.
fn read_style(path: &Path) -> Result<(Vec<f32>, Vec<f32>), SupertonicError> {
    let document = read_json(path)?;
    let tensor = |name: &str| -> Result<Vec<f32>, SupertonicError> {
        let data = document
            .pointer(&format!("/{name}/data"))
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                SupertonicError::Config(format!("{name} is missing from {}", path.display()))
            })?;
        let mut flat = Vec::new();
        // `[1, 50, 256]` and `[1, 8, 16]`: the batch dimension is always one, so the nesting
        // is walked rather than fixed, and a style with another shape still flattens.
        let mut stack: Vec<&serde_json::Value> = data.iter().collect();
        stack.reverse();
        while let Some(value) = stack.pop() {
            match value {
                serde_json::Value::Array(items) => stack.extend(items.iter().rev()),
                other => {
                    let number = other.as_f64().ok_or_else(|| {
                        SupertonicError::Config(format!(
                            "{name} holds a value that is not a number"
                        ))
                    })?;
                    flat.push(number as f32);
                }
            }
        }
        Ok(flat)
    };
    let ttl = tensor("style_ttl")?;
    let dp = tensor("style_dp")?;
    if ttl.len() != 50 * 256 {
        return Err(SupertonicError::Config(format!(
            "{} holds {} style values, and the encoder reads {}",
            path.display(),
            ttl.len(),
            50 * 256
        )));
    }
    if dp.len() != 8 * 16 {
        return Err(SupertonicError::Config(format!(
            "{} holds {} duration style values, and the predictor reads {}",
            path.display(),
            dp.len(),
            8 * 16
        )));
    }
    Ok((ttl, dp))
}

fn read_json(path: &Path) -> Result<serde_json::Value, SupertonicError> {
    let text = std::fs::read_to_string(path).map_err(|source| SupertonicError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&text)
        .map_err(|error| SupertonicError::Config(format!("{}: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_named() {
        let error = read_json(Path::new("/nonexistent/tts.json")).unwrap_err();
        assert!(error.to_string().contains("tts.json"), "{error}");
    }

    #[test]
    fn a_style_that_is_not_the_shape_the_graphs_read_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("style.json");
        std::fs::write(
            &path,
            r#"{"style_ttl": {"data": [[[0.0, 1.0]]]}, "style_dp": {"data": [[[0.0]]]}}"#,
        )
        .unwrap();
        let error = read_style(&path).unwrap_err();
        assert!(error.to_string().contains("style values"), "{error}");
    }

    #[test]
    fn a_language_the_model_does_not_speak_is_refused_before_a_graph_loads() {
        let error = Voice::load_with_threads(
            &Files {
                text_encoder: Path::new("/nonexistent"),
                duration_predictor: Path::new("/nonexistent"),
                vector_estimator: Path::new("/nonexistent"),
                vocoder: Path::new("/nonexistent"),
                config: Path::new("/nonexistent"),
                indexer: Path::new("/nonexistent"),
                style: Path::new("/nonexistent"),
            },
            "xx",
            1,
        )
        .unwrap_err();
        assert!(matches!(error, SupertonicError::Config(_)), "{error}");
    }
}
