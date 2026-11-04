//! Writing the rendered audio to a file.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use flacenc::component::BitRepr as _;
use flacenc::error::Verify as _;

/// What to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Uncompressed, 16-bit, one channel.
    Wav,
    /// Lossless and compressed, which is the smaller file for speech.
    Flac,
}

impl Format {
    /// The extension, without the dot.
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Flac => "flac",
        }
    }
}

/// Write `samples` at `rate` to `path`.
///
/// # Errors
///
/// The file cannot be written, or the encoder refused the samples.
pub fn save(path: &Path, format: Format, samples: &[f32], rate: u32) -> Result<()> {
    match format {
        Format::Wav => {
            let pcm = arioso_tts_piper::wav::pcm_s16le(samples);
            let bytes = arioso_tts_piper::wav::to_bytes_from_pcm(&pcm, rate);
            std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
        }
        Format::Flac => {
            let pcm: Vec<i32> = samples
                .iter()
                .map(|&sample| (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i32)
                .collect();
            let config = flacenc::config::Encoder::default()
                .into_verified()
                .map_err(|error| anyhow!("the encoder refused its own defaults: {error:?}"))?;
            let source = flacenc::source::MemSource::from_samples(&pcm, 1, 16, rate as usize);
            let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
                .map_err(|error| anyhow!("encoding FLAC: {error}"))?;
            let mut sink = flacenc::bitsink::ByteSink::new();
            stream
                .write(&mut sink)
                .map_err(|error| anyhow!("writing the FLAC stream: {error}"))?;
            std::fs::write(path, sink.as_slice())
                .with_context(|| format!("writing {}", path.display()))?;
        }
    }
    Ok(())
}

/// A file name for the utterance, in the current directory.
///
/// The first few words, so a folder of saved lines is readable without opening any of them.
#[must_use]
pub fn suggested_path(text: &str, format: Format) -> std::path::PathBuf {
    let mut slug = String::new();
    for word in text.split_whitespace().take(5) {
        let cleaned: String = word
            .chars()
            .filter(|character| character.is_alphanumeric())
            .collect::<String>()
            .to_lowercase();
        if cleaned.is_empty() {
            continue;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&cleaned);
    }
    if slug.is_empty() {
        slug.push_str("arioso");
    }
    std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join(format!("{slug}.{}", format.extension()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_suggested_name_is_the_first_words() {
        let path = suggested_path("Arioso gives your apps a voice and ears", Format::Wav);
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            "arioso-gives-your-apps-a.wav"
        );
    }

    #[test]
    fn a_name_with_nothing_usable_falls_back() {
        let path = suggested_path("—  ", Format::Flac);
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), "arioso.flac");
    }
}
