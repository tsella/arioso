//! Model manifest, cache layout, and resumable downloads for arioso.

pub mod cache;
pub mod download;
pub mod manifest;

pub use cache::{data_dir, models_dir};
pub use download::{DownloadError, Downloader, Progress, hash_file};
pub use manifest::{
    CheckedManifest, FileSpec, Kind, Manifest, ManifestError, Mode, ModelSpec, RELEASE_PUBKEY_HEX,
    SignatureState, signature_path, verify_manifest,
};
