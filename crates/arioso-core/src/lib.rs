//! Shared text handling for Arioso.

pub mod chunk;
pub mod dictionary;
pub mod normalize;

pub use chunk::{chunk_text, sentences};
pub use dictionary::{DictError, Entry, Piece, UserDictionary};
pub use normalize::{Options, normalize, normalize_with};
