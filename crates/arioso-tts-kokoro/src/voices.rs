//! The voice file: one style vector table per voice, in a numpy `.npz` archive.
//!
//! `voices-v1.0.bin` is `numpy.savez` output: a zip archive whose entries are `.npy`
//! arrays, stored rather than compressed. Only that shape is read here, because that is
//! the shape the model ships. A compressed entry is refused by name instead of being
//! decompressed with a dependency the rest of the program does not use.
//!
//! One voice is 511 rows of 256 floats, half a megabyte, and there are dozens, so the
//! archive is indexed once and a style is read from the file when it is selected. The
//! alternative — holding the whole archive — costs 28 MB for one voice's worth of use.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::KokoroError;

/// One voice's style table: one row per token length, one column per style dimension.
#[derive(Debug, Clone, PartialEq)]
pub struct Style {
    /// Rows, indexed by the token count of the run being spoken.
    pub rows: usize,
    /// Floats per row.
    pub width: usize,
    /// Row-major data.
    pub data: Vec<f32>,
}

impl Style {
    /// The row for a run of `tokens` symbols.
    ///
    /// The table has one row per length the model was trained on, and the last row is
    /// the longest; a run past the end uses it, which is what the reference
    /// implementation does.
    #[must_use]
    pub fn row(&self, tokens: usize) -> &[f32] {
        let index = tokens.min(self.rows.saturating_sub(1));
        let start = index * self.width;
        &self.data[start..start + self.width]
    }
}

/// Where one entry's bytes sit in the file.
#[derive(Debug, Clone)]
struct Entry {
    offset: u64,
    size: u64,
}

/// An indexed voice archive. Holds no audio data: only where each voice's bytes are.
#[derive(Debug)]
pub struct VoiceFile {
    path: PathBuf,
    entries: BTreeMap<String, Entry>,
}

impl VoiceFile {
    /// Index an archive by reading its entry headers.
    ///
    /// # Errors
    ///
    /// Returns [`KokoroError::Io`] when the file cannot be read, and
    /// [`KokoroError::Voices`] when it is not the archive this loader understands.
    pub fn open(path: &Path) -> Result<Self, KokoroError> {
        let mut file = File::open(path).map_err(|source| KokoroError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let length = file
            .metadata()
            .map_err(|source| KokoroError::Io {
                path: path.to_path_buf(),
                source,
            })?
            .len();
        let mut entries = BTreeMap::new();
        let mut position = 0u64;
        while position + 30 <= length {
            file.seek(SeekFrom::Start(position))
                .map_err(|source| KokoroError::Io {
                    path: path.to_path_buf(),
                    source,
                })?;
            let mut header = [0u8; 30];
            if file.read_exact(&mut header).is_err() {
                break;
            }
            // The central directory and the end-of-archive record are not entries.
            if header[..4] != *b"PK\x03\x04" {
                break;
            }
            let method = u16::from_le_bytes([header[8], header[9]]);
            let name_len = usize::from(u16::from_le_bytes([header[26], header[27]]));
            let extra_len = usize::from(u16::from_le_bytes([header[28], header[29]]));
            let mut name = vec![0u8; name_len];
            file.read_exact(&mut name)
                .map_err(|source| KokoroError::Io {
                    path: path.to_path_buf(),
                    source,
                })?;
            let mut extra = vec![0u8; extra_len];
            file.read_exact(&mut extra)
                .map_err(|source| KokoroError::Io {
                    path: path.to_path_buf(),
                    source,
                })?;
            let name = String::from_utf8_lossy(&name).into_owned();
            let (compressed, uncompressed) = sizes(&header, &extra);
            if method != 0 {
                return Err(KokoroError::Voices(format!(
                    "{name} is compressed; this loader reads the stored archive the model ships"
                )));
            }
            let offset = position + 30 + name_len as u64 + extra_len as u64;
            let stem = name
                .strip_suffix(".npy")
                .unwrap_or(&name)
                .rsplit('/')
                .next()
                .unwrap_or(&name)
                .to_string();
            entries.insert(
                stem,
                Entry {
                    offset,
                    size: compressed.min(uncompressed),
                },
            );
            position = offset + compressed.max(uncompressed);
        }
        if entries.is_empty() {
            return Err(KokoroError::Voices(format!(
                "{} holds no .npy entries; expected the voices-v1.0 archive",
                path.display()
            )));
        }
        Ok(Self {
            path: path.to_path_buf(),
            entries,
        })
    }

    /// The voice names in the archive, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    /// Whether the archive holds a voice.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// Read one voice's style table.
    ///
    /// # Errors
    ///
    /// Returns [`KokoroError::UnknownVoice`] for a name the archive does not hold, and
    /// [`KokoroError::Voices`] when the entry is not a little-endian `f4` array.
    pub fn style(&self, name: &str) -> Result<Style, KokoroError> {
        let entry = self
            .entries
            .get(name)
            .ok_or_else(|| KokoroError::UnknownVoice {
                name: name.to_string(),
                available: self.names(),
            })?;
        let mut file = File::open(&self.path).map_err(|source| KokoroError::Io {
            path: self.path.clone(),
            source,
        })?;
        file.seek(SeekFrom::Start(entry.offset))
            .map_err(|source| KokoroError::Io {
                path: self.path.clone(),
                source,
            })?;
        let mut bytes = vec![0u8; usize::try_from(entry.size).unwrap_or(0)];
        file.read_exact(&mut bytes)
            .map_err(|source| KokoroError::Io {
                path: self.path.clone(),
                source,
            })?;
        parse_npy(&bytes)
    }
}

/// The two sizes, from the header or from a ZIP64 extra field when the header says
/// "look further".
fn sizes(header: &[u8; 30], extra: &[u8]) -> (u64, u64) {
    let compressed = u64::from(u32::from_le_bytes([
        header[18], header[19], header[20], header[21],
    ]));
    let uncompressed = u64::from(u32::from_le_bytes([
        header[22], header[23], header[24], header[25],
    ]));
    if compressed != u32::MAX as u64 && uncompressed != u32::MAX as u64 {
        return (compressed, uncompressed);
    }
    // ZIP64: field id 0x0001, then the uncompressed size and the compressed size, each
    // eight bytes and each present only if the fixed header could not hold it.
    let mut cursor = 0;
    while cursor + 4 <= extra.len() {
        let id = u16::from_le_bytes([extra[cursor], extra[cursor + 1]]);
        let len = usize::from(u16::from_le_bytes([extra[cursor + 2], extra[cursor + 3]]));
        let body = &extra[cursor + 4..(cursor + 4 + len).min(extra.len())];
        if id == 0x0001 {
            let mut at = 0;
            let mut take = |size: u64| -> u64 {
                if size != u32::MAX as u64 {
                    return size;
                }
                let value = body
                    .get(at..at + 8)
                    .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8])))
                    .unwrap_or(0);
                at += 8;
                value
            };
            let uncompressed = take(uncompressed);
            let compressed = take(compressed);
            return (compressed, uncompressed);
        }
        cursor += 4 + len;
    }
    (compressed, uncompressed)
}

/// Read the one `.npy` shape the archive holds: little-endian `f4`, C order, shape
/// `(rows, 1, width)`.
fn parse_npy(bytes: &[u8]) -> Result<Style, KokoroError> {
    if bytes.len() < 10 || bytes[..6] != [0x93, b'N', b'U', b'M', b'P', b'Y'] {
        return Err(KokoroError::Voices("entry is not a .npy array".into()));
    }
    let (header_len, prefix) = match bytes[6] {
        1 => (
            usize::from(u16::from_le_bytes([bytes[8], bytes[9]])),
            10usize,
        ),
        2 | 3 => (
            usize::try_from(u32::from_le_bytes([
                bytes[8], bytes[9], bytes[10], bytes[11],
            ]))
            .unwrap_or(0),
            12,
        ),
        other => {
            return Err(KokoroError::Voices(format!(
                ".npy format version {other} is not supported"
            )));
        }
    };
    let header = bytes
        .get(prefix..prefix + header_len)
        .ok_or_else(|| KokoroError::Voices(".npy header runs past the entry".into()))?;
    let header = String::from_utf8_lossy(header);
    if !header.contains("'<f4'") {
        return Err(KokoroError::Voices(format!(
            "style array is not little-endian f4: {header}"
        )));
    }
    if header.contains("'fortran_order': True") {
        return Err(KokoroError::Voices(
            "style array is column-major, which this loader does not reorder".into(),
        ));
    }
    let shape = header
        .split("'shape':")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .map(|rest| {
            rest.trim_start_matches(|c: char| !c.is_ascii_digit() && c != '(')
                .trim_start_matches('(')
                .split(',')
                .filter_map(|part| part.trim().parse::<usize>().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let (rows, width) = match shape.as_slice() {
        [rows, 1, width] => (*rows, *width),
        [rows, width] => (*rows, *width),
        other => {
            return Err(KokoroError::Voices(format!(
                "style array has shape {other:?}; expected (rows, 1, width)"
            )));
        }
    };
    let start = prefix + header_len;
    let expected = rows * width * 4;
    let data = bytes
        .get(start..start + expected)
        .ok_or_else(|| KokoroError::Voices("style array is shorter than its shape".into()))?;
    let data = data
        .chunks_exact(4)
        .map(|word| f32::from_le_bytes([word[0], word[1], word[2], word[3]]))
        .collect();
    Ok(Style { rows, width, data })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A stored zip holding one `.npy` array, the way `numpy.savez` writes it.
    fn archive(entries: &[(&str, &[f32], usize, usize)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, data, rows, width) in entries {
            let header = format!(
                "{{'descr': '<f4', 'fortran_order': False, 'shape': ({rows}, 1, {width}), }}"
            );
            let mut npy = vec![0x93, b'N', b'U', b'M', b'P', b'Y', 1, 0];
            npy.extend_from_slice(&(header.len() as u16).to_le_bytes());
            npy.extend_from_slice(header.as_bytes());
            for sample in data.iter() {
                npy.extend_from_slice(&sample.to_le_bytes());
            }
            let file_name = format!("{name}.npy");
            let offset = out.len() as u32;
            let mut local = Vec::new();
            local.extend_from_slice(b"PK\x03\x04");
            local.extend_from_slice(&20u16.to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(&[0; 4]);
            local.extend_from_slice(&0u32.to_le_bytes());
            local.extend_from_slice(&(npy.len() as u32).to_le_bytes());
            local.extend_from_slice(&(npy.len() as u32).to_le_bytes());
            local.extend_from_slice(&(file_name.len() as u16).to_le_bytes());
            local.extend_from_slice(&0u16.to_le_bytes());
            local.extend_from_slice(file_name.as_bytes());
            local.extend_from_slice(&npy);
            out.extend_from_slice(&local);

            central.extend_from_slice(b"PK\x01\x02");
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&[0; 4]);
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&(npy.len() as u32).to_le_bytes());
            central.extend_from_slice(&(npy.len() as u32).to_le_bytes());
            central.extend_from_slice(&(file_name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(file_name.as_bytes());
        }
        let central_at = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&central_at.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn written(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voices.bin");
        let mut file = File::create(&path).unwrap();
        file.write_all(bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn a_voice_is_named_and_its_style_is_read_back() {
        let rows = 3;
        let width = 2;
        let data: Vec<f32> = (0..(rows * width) as i32).map(|n| n as f32).collect();
        let (_dir, path) = written(&archive(&[("af_heart", &data, rows, width)]));
        let voices = VoiceFile::open(&path).unwrap();
        assert_eq!(voices.names(), vec!["af_heart".to_string()]);
        assert!(voices.contains("af_heart"));
        let style = voices.style("af_heart").unwrap();
        assert_eq!((style.rows, style.width), (rows, width));
        assert_eq!(style.row(1), &[2.0, 3.0]);
    }

    #[test]
    fn a_run_past_the_table_uses_the_last_row() {
        let style = Style {
            rows: 2,
            width: 2,
            data: vec![0.0, 1.0, 2.0, 3.0],
        };
        assert_eq!(style.row(0), &[0.0, 1.0]);
        assert_eq!(style.row(99), &[2.0, 3.0]);
    }

    #[test]
    fn an_unknown_voice_names_the_ones_that_exist() {
        let data = vec![0.0f32; 2];
        let (_dir, path) = written(&archive(&[
            ("af_heart", &data, 1, 2),
            ("am_michael", &data, 1, 2),
        ]));
        let voices = VoiceFile::open(&path).unwrap();
        let error = voices.style("xx_nobody").unwrap_err().to_string();
        assert!(error.contains("af_heart"), "{error}");
        assert!(error.contains("am_michael"), "{error}");
    }

    #[test]
    fn a_file_that_is_not_an_archive_is_refused() {
        let (_dir, path) = written(b"not a zip at all");
        assert!(VoiceFile::open(&path).is_err());
    }
}
