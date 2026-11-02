//! Model manifest: what a model is, where its files come from, and how to trust them.

use std::path::Path;

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

    /// Load a manifest from disk.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Io`] or [`ManifestError::Parse`].
    pub fn load(path: &Path) -> Result<Self, ManifestError> {
        let bytes = std::fs::read(path).map_err(|source| ManifestError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&bytes)
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
    #[error("manifest signature does not verify")]
    BadSignature,
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
        .map_err(|_| ManifestError::BadSignature)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

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
        assert!(matches!(err, ManifestError::BadSignature));
    }

    #[test]
    fn missing_release_key_is_named() {
        let err = verify_manifest(b"x", "00", RELEASE_PUBKEY_HEX).unwrap_err();
        assert!(err.to_string().contains("no release public key"), "{err}");
    }

    #[test]
    fn malformed_signature_material_is_reported() {
        let err = verify_manifest(b"x", "zz", "00").unwrap_err();
        assert!(matches!(err, ManifestError::Malformed(_)));
    }
}
