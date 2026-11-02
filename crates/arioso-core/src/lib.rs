//! Shared text handling for Arioso.

pub mod chunk;
pub mod normalize;

pub use chunk::{chunk_text, sentences};
pub use normalize::normalize;
