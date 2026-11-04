//! Minting a signing key and applying it to a manifest.
//!
//! The verification half lives in [`crate::manifest`]; this is the operator's half. One
//! keypair, one detached signature over the manifest's bytes exactly as they sit on disk,
//! and nothing else. Nothing here runs at server start: a key is an operator's act on the
//! machine that holds the release, not something a build does on its own.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signer, SigningKey};
use rand_core::OsRng;
use thiserror::Error;

/// Size of the private key file: one raw Ed25519 seed, no encoding and no header.
pub const PRIVATE_KEY_LEN: usize = 32;

/// Why a key could not be minted or a manifest could not be signed.
#[derive(Debug, Error)]
pub enum SigningError {
    /// A file already sits where the private key would go.
    #[error("refusing to overwrite the existing file at {path}")]
    KeyExists {
        /// The file that was already there.
        path: PathBuf,
    },
    /// A file could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// File that failed.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
    /// The file is not a manifest, so a signature over it would prove nothing.
    #[error("{path} is not a manifest: {detail}")]
    NotAManifest {
        /// The file that was read.
        path: PathBuf,
        /// What the parser said.
        detail: String,
    },
    /// The key file is not one raw 32-byte seed.
    #[error("{path} is not a private key: expected {PRIVATE_KEY_LEN} raw bytes, found {found}")]
    BadKey {
        /// The key file that was read.
        path: PathBuf,
        /// Its actual length.
        found: usize,
    },
}

/// A signature that was written, and the public key that made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedManifest {
    /// The `.sig` file that now sits beside the manifest.
    pub signature_path: PathBuf,
    /// The signing key's public half, as 64 lowercase hex characters.
    pub public_key_hex: String,
}

/// Mint an Ed25519 keypair, write the private half to `path`, and return the public half.
///
/// The file is created with `create_new`, so an existing path is refused rather than
/// truncated: overwriting a key that already signed a release destroys the only copy of
/// something still needed to reproduce that signature.
///
/// On unix the file is created with mode `0600` and the mode is re-applied after the write,
/// so a permissive umask cannot leave a readable window. On Windows there is no file mode
/// to set: the key inherits the access control list of the directory it is written to, and
/// the operator is responsible for choosing a directory only their account can read. The
/// refusal to overwrite still holds there, because `create_new` is not a unix facility.
///
/// # Errors
///
/// Returns [`SigningError::KeyExists`] when `path` is taken, and [`SigningError::Io`] when
/// the file cannot be created or written.
pub fn generate_keypair(path: &Path) -> Result<String, SigningError> {
    let signing = SigningKey::generate(&mut OsRng);
    let mut file = create_private_file(path)?;
    file.write_all(&signing.to_bytes())
        .and_then(|()| file.flush())
        .map_err(|source| SigningError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    drop(file);
    // `OpenOptions::mode` is filtered through the umask, so a mask of 077 would leave the
    // key at 0600 by luck and 0777 would leave it at 0000. Setting it again is exact.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
            |source| SigningError::Io {
                path: path.to_path_buf(),
                source,
            },
        )?;
    }
    Ok(hex::encode(signing.verifying_key().to_bytes()))
}

/// Create the private key file, or say that the path is taken.
fn create_private_file(path: &Path) -> Result<std::fs::File, SigningError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // Set at creation rather than after it: a key that exists for one instant at 0644
        // is a key that leaked.
        options.mode(0o600);
    }
    options.open(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::AlreadyExists {
            SigningError::KeyExists {
                path: path.to_path_buf(),
            }
        } else {
            SigningError::Io {
                path: path.to_path_buf(),
                source,
            }
        }
    })
}

/// Sign a manifest file and write the detached signature beside it.
///
/// The bytes signed are the bytes on disk, read once and never re-serialized: re-encoding
/// the JSON first would make a whitespace change break verification for no security gain.
/// The signature file is `<manifest>.sig`, lowercase hex, one trailing newline.
///
/// # Errors
///
/// Returns [`SigningError::Io`] when the manifest or the key cannot be read or the signature
/// cannot be written, and [`SigningError::BadKey`] when the key file is not 32 raw bytes.
pub fn sign_manifest_file(manifest: &Path, key: &Path) -> Result<SignedManifest, SigningError> {
    let signing = read_private_key(key)?;
    let bytes = std::fs::read(manifest).map_err(|source| SigningError::Io {
        path: manifest.to_path_buf(),
        source,
    })?;
    // The bytes that get signed are the bytes on disk, but a file that is not a manifest is
    // not a release: signing one would put a valid signature on a document that cannot load.
    crate::manifest::Manifest::parse(&bytes).map_err(|error| SigningError::NotAManifest {
        path: manifest.to_path_buf(),
        detail: error.to_string(),
    })?;
    let signature = hex::encode(signing.sign(&bytes).to_bytes());
    let signature_path = crate::manifest::signature_path(manifest);
    // Written beside the target and renamed, so a reader sees the old signature or the new
    // one and never half a file.
    let temporary = signature_path.with_extension("sig.tmp");
    std::fs::write(&temporary, format!("{signature}\n")).map_err(|source| SigningError::Io {
        path: temporary.clone(),
        source,
    })?;
    std::fs::rename(&temporary, &signature_path).map_err(|source| SigningError::Io {
        path: signature_path.clone(),
        source,
    })?;
    Ok(SignedManifest {
        signature_path,
        public_key_hex: hex::encode(signing.verifying_key().to_bytes()),
    })
}

/// Read the 32 raw bytes of a private key file.
fn read_private_key(path: &Path) -> Result<SigningKey, SigningError> {
    let bytes = std::fs::read(path).map_err(|source| SigningError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let seed: [u8; PRIVATE_KEY_LEN] =
        bytes
            .as_slice()
            .try_into()
            .map_err(|_| SigningError::BadKey {
                path: path.to_path_buf(),
                found: bytes.len(),
            })?;
    Ok(SigningKey::from_bytes(&seed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Manifest, ManifestError, SignatureState, verify_manifest};

    const SAMPLE: &str = r#"{
        "schema_version": 1,
        "models": [{
            "id": "piper-en_US-lessac-medium",
            "kind": "tts",
            "engine": "piper",
            "version": "1.0.0",
            "license": "Blizzard-2013",
            "files": [{
                "name": "en_US-lessac-medium.onnx",
                "url": "https://example.invalid/voice.onnx",
                "sha256": "5efe09e69902187827af646e1a6e9d269dee769f9877d17b16b1b46eeaaf019f",
                "size": 63201294
            }],
            "meta": {"sample_rate": 22050}
        }]
    }"#;

    /// Write a manifest into `dir` and return its path.
    fn manifest_in(dir: &Path) -> PathBuf {
        let path = dir.join("manifest.json");
        std::fs::write(&path, SAMPLE).unwrap();
        path
    }

    #[test]
    fn keygen_writes_one_raw_seed_and_prints_the_public_half() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("release.key");
        let public = generate_keypair(&key).unwrap();
        assert_eq!(public.len(), 64);
        assert!(
            public
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        let written = std::fs::read(&key).unwrap();
        assert_eq!(written.len(), PRIVATE_KEY_LEN);
        // The public key printed is the one this seed produces, not some other key.
        let seed: [u8; PRIVATE_KEY_LEN] = written.as_slice().try_into().unwrap();
        let expected = hex::encode(SigningKey::from_bytes(&seed).verifying_key().to_bytes());
        assert_eq!(public, expected);
    }

    #[cfg(unix)]
    #[test]
    fn keygen_leaves_the_key_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("release.key");
        generate_keypair(&key).unwrap();
        let mode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{mode:o}");
    }

    #[test]
    fn keygen_refuses_to_overwrite_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("release.key");
        generate_keypair(&key).unwrap();
        let before = std::fs::read(&key).unwrap();
        let err = generate_keypair(&key).unwrap_err();
        assert!(matches!(err, SigningError::KeyExists { .. }), "{err:?}");
        assert!(
            err.to_string().contains(&key.display().to_string()),
            "{err}"
        );
        assert_eq!(
            std::fs::read(&key).unwrap(),
            before,
            "the key was rewritten"
        );
    }

    #[test]
    fn a_file_that_is_not_a_manifest_is_not_signed() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("release.key");
        generate_keypair(&key).unwrap();
        let path = dir.path().join("manifest.json");
        // Valid JSON, but not a manifest: the `models` field is the wrong type.
        std::fs::write(&path, br#"{"schema_version": 1, "models": "nope"}"#).unwrap();

        let error = sign_manifest_file(&path, &key).unwrap_err();
        assert!(
            matches!(error, SigningError::NotAManifest { .. }),
            "{error:?}"
        );
        assert!(
            error.to_string().contains("manifest.json"),
            "the refusal names the file: {error}"
        );
        assert!(
            !crate::manifest::signature_path(&path).exists(),
            "a refused signing writes no signature"
        );
    }

    #[test]
    fn signing_a_missing_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = manifest_in(dir.path());
        let err = sign_manifest_file(&manifest, &dir.path().join("absent.key")).unwrap_err();
        assert!(matches!(err, SigningError::Io { .. }), "{err:?}");
        assert!(err.to_string().contains("absent.key"), "{err}");
        assert!(!crate::manifest::signature_path(&manifest).exists());
    }

    #[test]
    fn a_key_file_of_the_wrong_size_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = manifest_in(dir.path());
        let key = dir.path().join("short.key");
        std::fs::write(&key, [1u8; 16]).unwrap();
        let err = sign_manifest_file(&manifest, &key).unwrap_err();
        assert!(
            matches!(err, SigningError::BadKey { found: 16, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_generated_key_signs_a_manifest_and_the_signature_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = manifest_in(dir.path());
        let key = dir.path().join("release.key");
        let public = generate_keypair(&key).unwrap();
        let signed = sign_manifest_file(&manifest, &key).unwrap();
        assert_eq!(signed.signature_path, dir.path().join("manifest.json.sig"));
        assert_eq!(signed.public_key_hex, public);
        let text = std::fs::read_to_string(&signed.signature_path).unwrap();
        verify_manifest(SAMPLE.as_bytes(), &text, &public).expect("the signature verifies");
        // The bytes on disk are what the signature covers, newline or no newline.
        let bytes = std::fs::read(&manifest).unwrap();
        verify_manifest(&bytes, text.trim(), &public).expect("the file's bytes verify");
    }

    #[test]
    fn one_flipped_byte_after_signing_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = manifest_in(dir.path());
        let key = dir.path().join("release.key");
        let public = generate_keypair(&key).unwrap();
        let signed = sign_manifest_file(&manifest, &key).unwrap();

        // Flip one byte of the manifest in place, keeping the document valid JSON so the
        // refusal cannot come from the parser.
        let mut bytes = std::fs::read(&manifest).unwrap();
        let at = bytes
            .windows(8)
            .position(|w| w == b"63201294")
            .expect("the size is in the document");
        bytes[at + 7] = b'5';
        std::fs::write(&manifest, &bytes).unwrap();

        let text = std::fs::read_to_string(&signed.signature_path).unwrap();
        let err = verify_manifest(&bytes, &text, &public).unwrap_err();
        assert!(matches!(err, ManifestError::BadSignature { .. }), "{err:?}");
        // And the whole load path refuses it, naming the signature file.
        let err = Manifest::load_checked(&manifest, &signed.signature_path, &public).unwrap_err();
        assert!(
            err.to_string()
                .contains(&signed.signature_path.display().to_string()),
            "{err}"
        );
    }

    #[test]
    fn a_signature_from_another_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = manifest_in(dir.path());
        let mine = dir.path().join("mine.key");
        let theirs = dir.path().join("theirs.key");
        let my_public = generate_keypair(&mine).unwrap();
        let their_public = generate_keypair(&theirs).unwrap();
        let signed = sign_manifest_file(&manifest, &theirs).unwrap();
        assert_ne!(my_public, their_public);

        let text = std::fs::read_to_string(&signed.signature_path).unwrap();
        let err = verify_manifest(SAMPLE.as_bytes(), &text, &my_public).unwrap_err();
        assert!(matches!(err, ManifestError::BadSignature { .. }), "{err:?}");
        let err =
            Manifest::load_checked(&manifest, &signed.signature_path, &my_public).unwrap_err();
        assert!(matches!(err, ManifestError::BadSignature { .. }), "{err:?}");
        // The key that made it still verifies, so the refusal is about the key, not the file.
        let checked =
            Manifest::load_checked(&manifest, &signed.signature_path, &their_public).unwrap();
        assert_eq!(checked.signature, SignatureState::Verified);
    }

    #[test]
    fn a_compiled_key_with_no_signature_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = manifest_in(dir.path());
        let key = dir.path().join("release.key");
        let public = generate_keypair(&key).unwrap();
        let err = Manifest::load_checked(&manifest, &dir.path().join("manifest.json.sig"), &public)
            .unwrap_err();
        assert!(
            matches!(err, ManifestError::SignatureMissing { .. }),
            "{err:?}"
        );
        assert!(
            err.to_string()
                .contains(&dir.path().join("manifest.json.sig").display().to_string()),
            "{err}"
        );
    }

    #[test]
    fn no_compiled_key_loads_the_same_document_as_unsigned() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = manifest_in(dir.path());
        let checked =
            Manifest::load_checked(&manifest, &dir.path().join("manifest.json.sig"), "").unwrap();
        assert_eq!(checked.signature, SignatureState::Unsigned);
        assert!(checked.manifest.find("piper-en_US-lessac-medium").is_some());
    }
}
