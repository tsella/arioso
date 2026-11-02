//! Resumable downloads with integrity checks.

use std::io::Read;
use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::AsyncWriteExt;

use crate::manifest::{FileSpec, ModelSpec};

/// Bytes fetched so far for one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Bytes on disk, including bytes carried over from an earlier attempt.
    pub downloaded: u64,
    /// Expected total from the manifest.
    pub total: u64,
}

impl Progress {
    /// Completion percentage, 0 to 100.
    #[must_use]
    pub fn percent(&self) -> f32 {
        if self.total == 0 {
            100.0
        } else {
            (self.downloaded as f64 / self.total as f64 * 100.0) as f32
        }
    }
}

/// Why a download failed.
#[derive(Debug, Error)]
pub enum DownloadError {
    /// The HTTP request failed.
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    /// Writing to the cache failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// The server refused the request.
    #[error("{file}: server returned status {status}")]
    Status {
        /// File being fetched.
        file: String,
        /// HTTP status code.
        status: u16,
    },
    /// The bytes on disk do not match the manifest hash.
    #[error("{file}: checksum mismatch, expected {expected}, got {actual}")]
    Integrity {
        /// File being fetched.
        file: String,
        /// Hash from the manifest.
        expected: String,
        /// Hash of what arrived.
        actual: String,
    },
    /// The byte count does not match the manifest.
    #[error("{file}: size mismatch, expected {expected} bytes, got {actual}")]
    Size {
        /// File being fetched.
        file: String,
        /// Size from the manifest.
        expected: u64,
        /// Size on disk.
        actual: u64,
    },
}

/// Fetches model files into a cache directory.
#[derive(Debug, Clone)]
pub struct Downloader {
    client: reqwest::Client,
}

impl Downloader {
    /// Build a downloader with a shared HTTP client.
    ///
    /// # Errors
    ///
    /// Returns [`DownloadError::Http`] when the client cannot be built.
    pub fn new() -> Result<Self, DownloadError> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("arioso/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { client })
    }

    /// Fetch every file of `spec` into `dir`, resuming partial files.
    ///
    /// `progress` is called after every chunk and once per already-complete file.
    ///
    /// # Errors
    ///
    /// Returns [`DownloadError`] on network failure, a bad status, a size
    /// mismatch, or a hash mismatch. A hash mismatch deletes the partial file.
    pub async fn ensure_model(
        &self,
        spec: &ModelSpec,
        dir: &Path,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<Vec<PathBuf>, DownloadError> {
        tokio::fs::create_dir_all(dir).await?;
        let mut paths = Vec::with_capacity(spec.files.len());
        for file in &spec.files {
            paths.push(self.ensure_file(file, dir, progress).await?);
        }
        Ok(paths)
    }

    async fn ensure_file(
        &self,
        file: &FileSpec,
        dir: &Path,
        progress: &mut dyn FnMut(Progress),
    ) -> Result<PathBuf, DownloadError> {
        let final_path = dir.join(&file.name);
        let part_path = dir.join(format!("{}.part", file.name));

        if final_path.exists() {
            let meta = std::fs::metadata(&final_path)?;
            if meta.len() == file.size && hash_file(&final_path)? == file.sha256 {
                progress(Progress {
                    downloaded: file.size,
                    total: file.size,
                });
                return Ok(final_path);
            }
        }

        let mut offset = std::fs::metadata(&part_path).map(|m| m.len()).unwrap_or(0);
        if offset > file.size {
            std::fs::remove_file(&part_path)?;
            offset = 0;
        }

        if offset < file.size {
            let mut request = self.client.get(&file.url);
            if offset > 0 {
                request = request.header(reqwest::header::RANGE, format!("bytes={offset}-"));
            }
            let response = request.send().await?;
            let status = response.status().as_u16();
            let append = match status {
                206 => true,
                200 => false,
                416 if offset == file.size => true,
                other => {
                    return Err(DownloadError::Status {
                        file: file.name.clone(),
                        status: other,
                    });
                }
            };

            let mut handle = if append && offset > 0 {
                tokio::fs::OpenOptions::new()
                    .append(true)
                    .open(&part_path)
                    .await?
            } else {
                offset = 0;
                tokio::fs::File::create(&part_path).await?
            };

            progress(Progress {
                downloaded: offset,
                total: file.size,
            });
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                handle.write_all(&chunk).await?;
                offset += chunk.len() as u64;
                progress(Progress {
                    downloaded: offset,
                    total: file.size,
                });
            }
            handle.flush().await?;
        }

        let actual = hash_file(&part_path)?;
        if actual != file.sha256 {
            let _ = std::fs::remove_file(&part_path);
            return Err(DownloadError::Integrity {
                file: file.name.clone(),
                expected: file.sha256.clone(),
                actual,
            });
        }
        let actual_size = std::fs::metadata(&part_path)?.len();
        if actual_size != file.size {
            return Err(DownloadError::Size {
                file: file.name.clone(),
                expected: file.size,
                actual: actual_size,
            });
        }
        tokio::fs::rename(&part_path, &final_path).await?;
        progress(Progress {
            downloaded: file.size,
            total: file.size,
        });
        Ok(final_path)
    }
}

/// SHA-256 of a file, lowercase hex.
///
/// # Errors
///
/// Returns the underlying I/O error.
pub fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{FileSpec, Kind, ModelSpec};
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn payload() -> Vec<u8> {
        (0..4096u32).map(|i| (i % 251) as u8).collect()
    }

    fn spec(url: String, data: &[u8]) -> ModelSpec {
        ModelSpec {
            id: "test-model".into(),
            kind: Kind::Tts,
            engine: "piper".into(),
            mode: None,
            version: "1.0.0".into(),
            license: "MIT".into(),
            commercial_use: true,
            files: vec![FileSpec {
                name: "voice.bin".into(),
                url,
                sha256: hex::encode(Sha256::digest(data)),
                size: data.len() as u64,
            }],
            meta: serde_json::Value::Null,
        }
    }

    #[tokio::test]
    async fn downloads_and_verifies() {
        let server = MockServer::start().await;
        let data = payload();
        Mock::given(method("GET"))
            .and(path("/voice.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(data.clone()))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let model = spec(format!("{}/voice.bin", server.uri()), &data);
        let mut seen = Vec::new();
        let paths = Downloader::new()
            .unwrap()
            .ensure_model(&model, dir.path(), &mut |p| seen.push(p.downloaded))
            .await
            .unwrap();

        assert_eq!(std::fs::read(&paths[0]).unwrap(), data);
        assert!(!dir.path().join("voice.bin.part").exists());
        assert_eq!(*seen.last().unwrap(), data.len() as u64);
        assert!(
            seen.windows(2).all(|w| w[0] <= w[1]),
            "progress must not go backwards"
        );
    }

    #[tokio::test]
    async fn resumes_from_partial_file() {
        let server = MockServer::start().await;
        let data = payload();
        let split = 1000usize;
        Mock::given(method("GET"))
            .and(path("/voice.bin"))
            .and(header_exists("range"))
            .respond_with(ResponseTemplate::new(206).set_body_bytes(data[split..].to_vec()))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("voice.bin.part"), &data[..split]).unwrap();
        let model = spec(format!("{}/voice.bin", server.uri()), &data);

        let paths = Downloader::new()
            .unwrap()
            .ensure_model(&model, dir.path(), &mut |_| {})
            .await
            .unwrap();
        assert_eq!(std::fs::read(&paths[0]).unwrap(), data);
    }

    #[tokio::test]
    async fn rejects_bad_hash_and_cleans_up() {
        let server = MockServer::start().await;
        let data = payload();
        let corrupt = vec![0u8; data.len()];
        Mock::given(method("GET"))
            .and(path("/voice.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(corrupt))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let model = spec(format!("{}/voice.bin", server.uri()), &data);
        let err = Downloader::new()
            .unwrap()
            .ensure_model(&model, dir.path(), &mut |_| {})
            .await
            .unwrap_err();

        assert!(matches!(err, DownloadError::Integrity { .. }), "{err:?}");
        assert!(!dir.path().join("voice.bin.part").exists());
        assert!(!dir.path().join("voice.bin").exists());
    }

    #[tokio::test]
    async fn skips_file_already_present() {
        let server = MockServer::start().await;
        let data = payload();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("voice.bin"), &data).unwrap();

        // No mock is mounted: any HTTP call would fail the test.
        let model = spec(format!("{}/voice.bin", server.uri()), &data);
        let paths = Downloader::new()
            .unwrap()
            .ensure_model(&model, dir.path(), &mut |_| {})
            .await
            .unwrap();
        assert_eq!(std::fs::read(&paths[0]).unwrap(), data);
    }
}
