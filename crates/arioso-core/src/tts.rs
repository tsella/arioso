//! What a speech engine has to provide.
//!
//! The engines in this tree differ in how text becomes symbols and how a voice is
//! chosen. They agree on two things: a sample rate, and one call that turns a run of
//! the engine's own symbol ids into mono samples. Phonemization, sentence splitting and
//! the dictionary live above this line, so a second engine is a file, not a fork.

use thiserror::Error;

/// Why an engine could not produce audio.
#[derive(Debug, Error)]
pub enum EngineError {
    /// The engine's runtime refused a step. The message is the engine's own, because
    /// the runtimes disagree about how to describe the same failure.
    #[error("{0}")]
    Backend(String),
    /// The graph ran and produced no audio.
    #[error("the model returned no audio")]
    NoOutput,
    /// The caller asked for a symbol the engine's table does not have.
    #[error("symbol {0} is not in this engine's table")]
    UnknownSymbol(i64),
}

/// One speech engine, as the rest of the program sees it.
///
/// Deliberately narrow: a caller that needs a speaker number, a style or a phoneme
/// table is talking to one engine, and should hold that engine's type.
pub trait TtsEngine {
    /// Sample rate of the audio this engine produces.
    fn sample_rate(&self) -> u32;

    /// Synthesize one run of the engine's own symbol ids.
    ///
    /// `speed` is a multiplier of the engine's own rate: 2.0 is twice as fast. One call
    /// is one utterance; text splitting is the caller's job, because the two engines
    /// want different amounts of it.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] when the runtime fails, the graph returns nothing, or a
    /// symbol is outside the table.
    fn render(&mut self, symbols: &[i64], speed: f32) -> Result<Vec<f32>, EngineError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in engine, to prove the trait is implementable without a runtime.
    struct Echo {
        rate: u32,
    }

    impl TtsEngine for Echo {
        fn sample_rate(&self) -> u32 {
            self.rate
        }

        fn render(&mut self, symbols: &[i64], speed: f32) -> Result<Vec<f32>, EngineError> {
            if symbols.contains(&-1) {
                return Err(EngineError::UnknownSymbol(-1));
            }
            Ok(symbols
                .iter()
                .map(|id| *id as f32 * speed)
                .collect::<Vec<_>>())
        }
    }

    #[test]
    fn the_trait_carries_the_rate_and_the_symbols() {
        let mut engine = Echo { rate: 24_000 };
        assert_eq!(engine.sample_rate(), 24_000);
        assert_eq!(engine.render(&[1, 2], 2.0).unwrap(), vec![2.0, 4.0]);
        assert!(matches!(
            engine.render(&[-1], 1.0),
            Err(EngineError::UnknownSymbol(-1))
        ));
    }
}
