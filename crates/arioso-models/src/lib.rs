//! Model manifest, cache layout, and resumable downloads for Arioso.

pub mod cache;
pub mod download;
pub mod manifest;

pub use cache::{data_dir, models_dir};
pub use download::{DownloadError, Downloader, Progress, hash_file};
pub use manifest::{FileSpec, Kind, Manifest, ManifestError, Mode, ModelSpec, verify_manifest};
