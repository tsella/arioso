//! Resumable downloads with integrity checks.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    /// There is not enough room on the target filesystem.
    #[error("need {needed} bytes in {dir}, {available} available")]
    InsufficientSpace {
        /// Bytes required.
        needed: u64,
        /// Bytes free.
        available: u64,
        /// Directory that would receive the files.
        dir: PathBuf,
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
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(60))
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
    pub async fn ensure_model<F>(
        &self,
        spec: &ModelSpec,
        dir: &Path,
        progress: &mut F,
    ) -> Result<Vec<PathBuf>, DownloadError>
    where
        F: FnMut(Progress) + Send,
    {
        tokio::fs::create_dir_all(dir).await?;
        let total: u64 = spec.files.iter().map(|f| f.size).sum();
        let needed = spec
            .files
            .iter()
            .map(|f| f.size.saturating_sub(existing_bytes(dir, &f.name)))
            .sum::<u64>();
        let available = fs4::available_space(dir)?;
        ensure_space(needed, available, dir)?;
        let mut done = 0u64;
        let mut paths = Vec::with_capacity(spec.files.len());
        for file in &spec.files {
            paths.push(self.ensure_file(file, dir, done, total, progress).await?);
            done += file.size;
        }
        Ok(paths)
    }

    /// `base` is the bytes of earlier files in the same model, so progress
    /// covers the whole model rather than the file in hand.
    async fn ensure_file<F>(
        &self,
        file: &FileSpec,
        dir: &Path,
        base: u64,
        total: u64,
        progress: &mut F,
    ) -> Result<PathBuf, DownloadError>
    where
        F: FnMut(Progress) + Send,
    {
        let final_path = dir.join(&file.name);
        let part_path = dir.join(format!("{}.part", file.name));

        if final_path.exists() {
            let meta = std::fs::metadata(&final_path)?;
            if meta.len() == file.size && hash_file_off_thread(&final_path).await? == file.sha256 {
                progress(Progress {
                    downloaded: base + file.size,
                    total,
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
                downloaded: base + offset,
                total,
            });
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                handle.write_all(&chunk).await?;
                offset += chunk.len() as u64;
                progress(Progress {
                    downloaded: base + offset,
                    total,
                });
            }
            handle.flush().await?;
        }

        let actual = hash_file_off_thread(&part_path).await?;
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
        // Windows refuses to rename onto an existing file, so clear a stale one first.
        if final_path.exists() {
            tokio::fs::remove_file(&final_path).await?;
        }
        tokio::fs::rename(&part_path, &final_path).await?;
        progress(Progress {
            downloaded: base + file.size,
            total,
        });
        Ok(final_path)
    }
}

/// Hash a file without holding a runtime worker.
///
/// The largest file in the catalog is 652 MB. Reading and hashing that on a Tokio worker
/// thread stalls every other task the runtime is serving for as long as it takes, and the
/// hash runs on both the resume path and the finish path.
async fn hash_file_off_thread(path: &Path) -> Result<String, DownloadError> {
    let path = path.to_owned();
    let hashed = tokio::task::spawn_blocking(move || hash_file(&path)).await;
    match hashed {
        Ok(result) => Ok(result?),
        Err(error) => Err(DownloadError::Io(std::io::Error::other(error))),
    }
}

/// Bytes already on disk for `name`, counting both a finished file and a partial one.
fn existing_bytes(dir: &Path, name: &str) -> u64 {
    let finished = std::fs::metadata(dir.join(name))
        .map(|m| m.len())
        .unwrap_or(0);
    let partial = std::fs::metadata(dir.join(format!("{name}.part")))
        .map(|m| m.len())
        .unwrap_or(0);
    finished.max(partial)
}

/// Refuse a download that cannot fit. Split out so it can be tested with numbers.
///
/// # Errors
///
/// Returns [`DownloadError::InsufficientSpace`] when `needed` exceeds `available`.
pub fn ensure_space(needed: u64, available: u64, dir: &Path) -> Result<(), DownloadError> {
    if needed > available {
        return Err(DownloadError::InsufficientSpace {
            needed,
            available,
            dir: dir.to_path_buf(),
        });
    }
    Ok(())
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

    #[test]
    fn space_check_compares_needed_with_available() {
        let dir = Path::new("/models");
        assert!(ensure_space(50, 50, dir).is_ok());
        let err = ensure_space(51, 50, dir).unwrap_err();
        assert!(matches!(
            err,
            DownloadError::InsufficientSpace {
                needed: 51,
                available: 50,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn refuses_a_model_that_cannot_fit() {
        let dir = tempfile::tempdir().unwrap();
        let mut model = spec("http://127.0.0.1:9/never".into(), b"x");
        model.files[0].size = u64::MAX / 2;

        // No mock is mounted: reaching the network would fail differently.
        let err = Downloader::new()
            .unwrap()
            .ensure_model(&model, dir.path(), &mut |_| {})
            .await
            .unwrap_err();
        assert!(
            matches!(err, DownloadError::InsufficientSpace { .. }),
            "{err:?}"
        );
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "no bytes written"
        );
    }

    #[tokio::test]
    async fn progress_covers_the_whole_model() {
        let server = MockServer::start().await;
        let a = payload();
        let b = b"sidecar".to_vec();
        Mock::given(method("GET"))
            .and(path("/a.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(a.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/b.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b.clone()))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let mut model = spec(format!("{}/a.bin", server.uri()), &a);
        model.files.push(FileSpec {
            name: "b.bin".into(),
            url: format!("{}/b.bin", server.uri()),
            sha256: hex::encode(Sha256::digest(&b)),
            size: b.len() as u64,
        });
        let total = a.len() as u64 + b.len() as u64;

        let mut seen = Vec::new();
        Downloader::new()
            .unwrap()
            .ensure_model(&model, dir.path(), &mut |p| seen.push(p))
            .await
            .unwrap();

        assert!(
            seen.iter().all(|p| p.total == total),
            "total must be the model size"
        );
        assert_eq!(seen.last().unwrap().downloaded, total);
        assert!(seen.windows(2).all(|w| w[0].downloaded <= w[1].downloaded));
    }

    #[tokio::test]
    async fn replaces_a_stale_file() {
        let server = MockServer::start().await;
        let data = payload();
        Mock::given(method("GET"))
            .and(path("/voice.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(data.clone()))
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("voice.bin"), b"older build of this voice").unwrap();
        let model = spec(format!("{}/voice.bin", server.uri()), &data);

        let paths = Downloader::new()
            .unwrap()
            .ensure_model(&model, dir.path(), &mut |_| {})
            .await
            .unwrap();
        assert_eq!(std::fs::read(&paths[0]).unwrap(), data);
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
