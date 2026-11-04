//! Model manifest: what a model is, where its files come from, and how to trust them.

use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Public key that signs the released manifest.
///
/// Empty until a release key exists. An empty key is an error, not a silent
/// verification failure, so a build cannot look signed when it is not.
pub const RELEASE_PUBKEY_HEX: &str = "";

/// What a model is used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Text to speech.
    Tts,
    /// Speech to text.
    Stt,
    /// Voice activity detection.
    Vad,
}

/// How a speech to text model consumes audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Whole files.
    Batch,
    /// Audio as it arrives.
    Streaming,
}

/// One downloadable file of a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSpec {
    /// File name inside the model directory.
    pub name: String,
    /// Source URL.
    pub url: String,
    /// Lowercase hex SHA-256 of the finished file.
    pub sha256: String,
    /// Expected size in bytes.
    pub size: u64,
}

/// One model in the catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelSpec {
    /// Stable id, used in config and on the command line.
    pub id: String,
    /// What the model is for.
    pub kind: Kind,
    /// Engine that loads it.
    pub engine: String,
    /// Batch or streaming, for speech to text.
    #[serde(default)]
    pub mode: Option<Mode>,
    /// Catalog version of this model.
    pub version: String,
    /// SPDX license id of the model weights.
    pub license: String,
    /// Whether the license allows commercial use.
    #[serde(default)]
    pub commercial_use: bool,
    /// Files that make up the model.
    pub files: Vec<FileSpec>,
    /// Engine specific metadata: sample rate, languages, speakers.
    #[serde(default)]
    pub meta: serde_json::Value,
}

impl ModelSpec {
    /// Total download size in bytes.
    #[must_use]
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// Whether every file is on disk at the size the catalog gives.
    ///
    /// Size, not hash: the hash is checked when a file is written, and checking 650 MB again
    /// on every request that asks whether a model is installed would cost more than the
    /// answer is worth.
    #[must_use]
    pub fn is_installed(&self, dir: &std::path::Path) -> bool {
        self.files.iter().all(|file| {
            std::fs::metadata(dir.join(&file.name))
                .map(|meta| meta.len() == file.size)
                .unwrap_or(false)
        })
    }

    /// Whether `voice` names this model, by id or by the bare voice name.
    ///
    /// The config may say `en_US-joe-medium`, which is the file stem, or
    /// `piper-en_US-joe-medium`, which is the catalog id. Both mean this model.
    #[must_use]
    pub fn is_voice(&self, voice: &str) -> bool {
        self.kind == Kind::Tts && (self.id == voice || self.id.ends_with(voice))
    }
}

impl Manifest {
    /// The voice a config names, by id or by the bare voice name.
    #[must_use]
    pub fn find_voice(&self, voice: &str) -> Option<&ModelSpec> {
        self.models
            .iter()
            .filter(|model| model.engine == "piper")
            .find(|model| model.is_voice(voice))
    }
}

/// What a manifest load established about the document's signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureState {
    /// No release public key is compiled into this build, so nothing could be checked.
    Unsigned,
    /// The detached signature verified against the compiled release key.
    Verified,
}

/// A manifest, and the signature state that was proved while loading it.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckedManifest {
    /// The parsed document.
    pub manifest: Manifest,
    /// Whether the signature was verified, or could not be checked at all.
    pub signature: SignatureState,
}

/// Path of the detached signature belonging to a manifest file.
///
/// The sibling takes the manifest's whole file name and appends `.sig`, so
/// `manifest.json` pairs with `manifest.json.sig`.
#[must_use]
pub fn signature_path(manifest: &Path) -> PathBuf {
    let mut name = manifest.as_os_str().to_owned();
    name.push(".sig");
    PathBuf::from(name)
}

/// The catalog document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Manifest format version.
    pub schema_version: u32,
    /// Every known model.
    pub models: Vec<ModelSpec>,
}

impl Manifest {
    /// Parse a manifest document.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Parse`] when the JSON does not match the schema.
    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
        let manifest: Self =
            serde_json::from_slice(bytes).map_err(|source| ManifestError::Parse { source })?;
        if manifest.schema_version != 1 {
            return Err(ManifestError::UnsupportedVersion {
                found: manifest.schema_version,
            });
        }
        manifest.validate()?;
        Ok(manifest)
    }

    /// Load a manifest from disk, checking its detached signature.
    ///
    /// The signature is the sibling `<path>.sig`, read as lowercase hex, and the key is
    /// [`RELEASE_PUBKEY_HEX`]. A build with no compiled key loads the manifest and cannot
    /// tell whether it was signed, which is why [`Self::load_checked`] reports the state
    /// it established.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Io`], [`ManifestError::Parse`],
    /// [`ManifestError::SignatureMissing`] or [`ManifestError::BadSignature`].
    pub fn load(path: &Path) -> Result<Self, ManifestError> {
        Self::load_checked(path, &signature_path(path), RELEASE_PUBKEY_HEX)
            .map(|checked| checked.manifest)
    }

    /// Load a manifest and enforce its detached signature.
    ///
    /// Three outcomes:
    ///
    /// - `pubkey_hex` is empty: the document is loaded and reported
    ///   [`SignatureState::Unsigned`]. No release key is compiled into this build, so there
    ///   is nothing to verify against.
    /// - A key is supplied and `signature_path` does not exist: refused with
    ///   [`ManifestError::SignatureMissing`], naming the file that was expected.
    /// - A key is supplied and the signature does not verify: refused with
    ///   [`ManifestError::BadSignature`], naming the file that was read.
    ///
    /// The bytes are verified before they are parsed, so a tampered document is reported as
    /// a signature failure rather than as whatever field the tampering broke.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Io`] when the manifest cannot be read,
    /// [`ManifestError::SignatureMissing`] when a key is present and no signature file is,
    /// [`ManifestError::BadSignature`] when the signature does not match, and
    /// [`ManifestError::Parse`] when the verified bytes are not a valid manifest.
    pub fn load_checked(
        path: &Path,
        signature_path: &Path,
        pubkey_hex: &str,
    ) -> Result<CheckedManifest, ManifestError> {
        let bytes = std::fs::read(path).map_err(|source| ManifestError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if pubkey_hex.trim().is_empty() {
            return Ok(CheckedManifest {
                manifest: Self::parse(&bytes)?,
                signature: SignatureState::Unsigned,
            });
        }
        let signature_hex = match std::fs::read_to_string(signature_path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(ManifestError::SignatureMissing {
                    path: signature_path.to_path_buf(),
                });
            }
            Err(source) => {
                return Err(ManifestError::Io {
                    path: signature_path.to_path_buf(),
                    source,
                });
            }
        };
        verify_manifest(&bytes, &signature_hex, pubkey_hex).map_err(|error| match error {
            ManifestError::BadSignature { .. } => ManifestError::BadSignature {
                path: Some(signature_path.to_path_buf()),
            },
            other => other,
        })?;
        Ok(CheckedManifest {
            manifest: Self::parse(&bytes)?,
            signature: SignatureState::Verified,
        })
    }

    fn validate(&self) -> Result<(), ManifestError> {
        let mut seen = std::collections::HashSet::new();
        for model in &self.models {
            if model.id.trim().is_empty() {
                return Err(ManifestError::Invalid {
                    path: "models[].id".into(),
                    message: "must not be empty".into(),
                });
            }
            if !seen.insert(model.id.as_str()) {
                return Err(ManifestError::Invalid {
                    path: format!("models[{}].id", model.id),
                    message: "duplicate model id".into(),
                });
            }
            for file in &model.files {
                // The name is joined onto the models directory. A name that climbs out of
                // it writes wherever the process can reach, which turns a corrupted
                // manifest into an arbitrary file write. The manifest ships with the
                // binary, so this needs local access to exploit, but the join is checked
                // here rather than trusted.
                if file.name.is_empty()
                    || file.name.contains('/')
                    || file.name.contains('\\')
                    || file.name.contains("..")
                    || std::path::Path::new(&file.name).is_absolute()
                {
                    return Err(ManifestError::Invalid {
                        path: format!("models[{}].files[{}].name", model.id, file.name),
                        message: "must be a plain file name, with no directory part".into(),
                    });
                }
                if !is_sha256(&file.sha256) {
                    return Err(ManifestError::Invalid {
                        path: format!("models[{}].files[{}].sha256", model.id, file.name),
                        message: "expected 64 lowercase hex characters".into(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Find a model by id.
    #[must_use]
    pub fn find(&self, id: &str) -> Option<&ModelSpec> {
        self.models.iter().find(|m| m.id == id)
    }
}

/// Why a manifest could not be loaded or trusted.
#[derive(Debug, Error)]
pub enum ManifestError {
    /// The file could not be read.
    #[error("reading {path}: {source}")]
    Io {
        /// File that failed.
        path: std::path::PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
    /// A field is present but not usable.
    #[error("invalid manifest at {path}: {message}")]
    Invalid {
        /// Dotted path of the offending value.
        path: String,
        /// What is wrong with it.
        message: String,
    },
    /// The document is not a valid manifest.
    #[error("parsing manifest: {source}")]
    Parse {
        /// Underlying error.
        source: serde_json::Error,
    },
    /// The document version is not supported by this build.
    #[error("unsupported manifest schema_version {found}")]
    UnsupportedVersion {
        /// Version found in the document.
        found: u32,
    },
    /// The signature does not match the manifest bytes.
    #[error("manifest signature does not verify{}", signature_file(path))]
    BadSignature {
        /// Signature file that was read, when the check ran against one.
        path: Option<PathBuf>,
    },
    /// A key is compiled in but the manifest's signature file is not there.
    #[error("manifest signature is missing: {path}")]
    SignatureMissing {
        /// File that was expected.
        path: PathBuf,
    },
    /// The key or signature is not valid hex or not a valid curve point.
    #[error("malformed signature material: {0}")]
    Malformed(String),
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Verify a detached signature over the manifest bytes.
///
/// `pubkey_hex` and `signature_hex` are lowercase hex. Ed25519, 32 and 64 bytes.
///
/// # Errors
///
/// Returns [`ManifestError::Malformed`] for bad encoding and
/// [`ManifestError::BadSignature`] when the signature does not verify.
pub fn verify_manifest(
    manifest_bytes: &[u8],
    signature_hex: &str,
    pubkey_hex: &str,
) -> Result<(), ManifestError> {
    if pubkey_hex.trim().is_empty() {
        return Err(ManifestError::Malformed(
            "no release public key is compiled in".into(),
        ));
    }
    let key_bytes: [u8; 32] = hex::decode(pubkey_hex.trim())
        .map_err(|e| ManifestError::Malformed(format!("public key: {e}")))?
        .try_into()
        .map_err(|_| ManifestError::Malformed("public key must be 32 bytes".into()))?;
    let key = VerifyingKey::from_bytes(&key_bytes)
        .map_err(|e| ManifestError::Malformed(format!("public key: {e}")))?;
    let sig_bytes = hex::decode(signature_hex.trim())
        .map_err(|e| ManifestError::Malformed(format!("signature: {e}")))?;
    let signature = Signature::from_slice(&sig_bytes)
        .map_err(|e| ManifestError::Malformed(format!("signature: {e}")))?;
    key.verify_strict(manifest_bytes, &signature)
        .map_err(|_| ManifestError::BadSignature { path: None })
}

/// Render the signature file for an error message, or nothing when the check had no file.
fn signature_file(path: &Option<PathBuf>) -> String {
    path.as_ref()
        .map(|path| format!(": {}", path.display()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn a_voice_is_found_by_its_id_or_its_bare_name() {
        // `tts.voice` may hold either, and `arioso doctor` reported an installed voice as
        // missing when it held the id, because it matched file names by substring.
        let manifest = Manifest::parse(include_bytes!("../../../models/manifest.json")).unwrap();
        let by_id = manifest.find_voice("piper-en_US-joe-medium");
        let by_name = manifest.find_voice("en_US-joe-medium");
        assert!(by_id.is_some());
        assert_eq!(by_id.map(|m| &m.id), by_name.map(|m| &m.id));
        assert!(manifest.find_voice("no-such-voice").is_none());
    }

    #[test]
    fn a_speech_model_is_not_a_voice() {
        let manifest = Manifest::parse(include_bytes!("../../../models/manifest.json")).unwrap();
        assert!(manifest.find_voice("sherpa-whisper-base-en").is_none());
    }

    const SAMPLE: &str = r#"{
        "schema_version": 1,
        "models": [{
            "id": "piper-en_US-lessac-medium",
            "kind": "tts",
            "engine": "piper",
            "version": "1.0.0",
            "license": "Blizzard-2013",
            "commercial_use": false,
            "files": [{
                "name": "en_US-lessac-medium.onnx",
                "url": "https://example.invalid/voice.onnx",
                "sha256": "5efe09e69902187827af646e1a6e9d269dee769f9877d17b16b1b46eeaaf019f",
                "size": 63201294
            }],
            "meta": {"sample_rate": 22050}
        }]
    }"#;

    fn keypair() -> (SigningKey, String) {
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let public = hex::encode(signing.verifying_key().to_bytes());
        (signing, public)
    }

    #[test]
    fn parses_and_finds() {
        let m = Manifest::parse(SAMPLE.as_bytes()).unwrap();
        let spec = m.find("piper-en_US-lessac-medium").expect("model present");
        assert_eq!(spec.kind, Kind::Tts);
        assert_eq!(spec.total_size(), 63_201_294);
        assert!(m.find("nope").is_none());
    }

    #[test]
    fn rejects_a_malformed_hash() {
        let bad = SAMPLE.replace(
            "5efe09e69902187827af646e1a6e9d269dee769f9877d17b16b1b46eeaaf019f",
            "aa",
        );
        let err = Manifest::parse(bad.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("sha256"), "{err}");
    }

    #[test]
    fn rejects_duplicate_ids() {
        let one = Manifest::parse(SAMPLE.as_bytes()).unwrap().models.remove(0);
        let doc = Manifest {
            schema_version: 1,
            models: vec![one.clone(), one],
        };
        let err = Manifest::parse(serde_json::to_string(&doc).unwrap().as_bytes()).unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn rejects_future_schema() {
        let err = Manifest::parse(br#"{"schema_version":2,"models":[]}"#).unwrap_err();
        assert!(matches!(
            err,
            ManifestError::UnsupportedVersion { found: 2 }
        ));
    }

    #[test]
    fn signature_round_trip() {
        let (signing, public) = keypair();
        let sig = hex::encode(signing.sign(SAMPLE.as_bytes()).to_bytes());
        verify_manifest(SAMPLE.as_bytes(), &sig, &public).expect("verifies");
    }

    #[test]
    fn tampered_manifest_fails() {
        let (signing, public) = keypair();
        let sig = hex::encode(signing.sign(SAMPLE.as_bytes()).to_bytes());
        let tampered = SAMPLE.replace("63201294", "63201295");
        let err = verify_manifest(tampered.as_bytes(), &sig, &public).unwrap_err();
        assert!(matches!(err, ManifestError::BadSignature { .. }));
    }

    #[test]
    fn missing_release_key_is_named() {
        let err = verify_manifest(b"x", "00", RELEASE_PUBKEY_HEX).unwrap_err();
        assert!(err.to_string().contains("no release public key"), "{err}");
    }

    #[test]
    fn a_file_name_that_climbs_out_of_the_directory_is_refused() {
        let hash = "0".repeat(64);
        for name in ["../../etc/passwd", "/etc/passwd", "sub/dir.bin", ".."] {
            let document = format!(
                r#"{{"schema_version":1,"models":[{{"id":"m","kind":"stt","engine":"sherpa","version":"1","license":"MIT","files":[{{"name":"{name}","url":"https://example.invalid/f","sha256":"{hash}","size":1}}]}}]}}"#
            );
            let error = Manifest::parse(document.as_bytes())
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("plain file name"),
                "{name} was accepted: {error}"
            );
        }
        // A plain name still parses, so the check rejects only what it should.
        let document = format!(
            r#"{{"schema_version":1,"models":[{{"id":"m","kind":"stt","engine":"sherpa","version":"1","license":"MIT","files":[{{"name":"model.onnx","url":"https://example.invalid/f","sha256":"{hash}","size":1}}]}}]}}"#
        );
        assert!(Manifest::parse(document.as_bytes()).is_ok());
    }

    #[test]
    fn malformed_signature_material_is_reported() {
        let err = verify_manifest(b"x", "zz", "00").unwrap_err();
        assert!(matches!(err, ManifestError::Malformed(_)));
    }

    fn write_signed(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf, String) {
        let (signing, public) = keypair();
        let manifest_path = dir.join("manifest.json");
        let signature_path = dir.join("manifest.json.sig");
        std::fs::write(&manifest_path, SAMPLE).unwrap();
        let signature = hex::encode(signing.sign(SAMPLE.as_bytes()).to_bytes());
        std::fs::write(&signature_path, &signature).unwrap();
        (manifest_path, signature_path, public)
    }

    #[test]
    fn a_signed_manifest_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let (manifest_path, signature_path, public) = write_signed(dir.path());
        let checked = Manifest::load_checked(&manifest_path, &signature_path, &public)
            .expect("the signature verifies");
        assert_eq!(checked.signature, SignatureState::Verified);
        assert!(checked.manifest.find("piper-en_US-lessac-medium").is_some());
    }

    #[test]
    fn one_flipped_byte_is_refused_and_names_the_signature_file() {
        let dir = tempfile::tempdir().unwrap();
        let (manifest_path, signature_path, public) = write_signed(dir.path());
        let tampered = SAMPLE.replace("63201294", "63201295");
        std::fs::write(&manifest_path, tampered).unwrap();
        let err = Manifest::load_checked(&manifest_path, &signature_path, &public).unwrap_err();
        assert!(matches!(err, ManifestError::BadSignature { .. }), "{err:?}");
        assert!(
            err.to_string()
                .contains(&signature_path.display().to_string()),
            "{err}"
        );
    }

    #[test]
    fn a_key_with_no_signature_file_is_refused_and_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        let signature_path = dir.path().join("manifest.json.sig");
        std::fs::write(&manifest_path, SAMPLE).unwrap();
        let (_, public) = keypair();
        let err = Manifest::load_checked(&manifest_path, &signature_path, &public).unwrap_err();
        assert!(
            matches!(err, ManifestError::SignatureMissing { .. }),
            "{err:?}"
        );
        assert!(
            err.to_string()
                .contains(&signature_path.display().to_string()),
            "{err}"
        );
    }

    #[test]
    fn no_key_loads_the_manifest_and_reports_unsigned() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        let signature_path = dir.path().join("manifest.json.sig");
        std::fs::write(&manifest_path, SAMPLE).unwrap();
        let checked = Manifest::load_checked(&manifest_path, &signature_path, "").unwrap();
        assert_eq!(checked.signature, SignatureState::Unsigned);
        assert!(checked.manifest.find("piper-en_US-lessac-medium").is_some());
    }

    #[test]
    fn load_uses_the_compiled_key_and_the_sibling_signature() {
        // The compiled key is empty in this build, so `load` reads the sibling `.sig`
        // without refusing, and reports nothing about it. When a release key exists this
        // same call is the one that refuses a bad signature.
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");
        std::fs::write(&manifest_path, SAMPLE).unwrap();
        std::fs::write(dir.path().join("manifest.json.sig"), "00").unwrap();
        let loaded = Manifest::load(&manifest_path).expect("no key is compiled in");
        assert!(loaded.find("piper-en_US-lessac-medium").is_some());
        assert_eq!(
            signature_path(&manifest_path),
            dir.path().join("manifest.json.sig")
        );
    }
}
