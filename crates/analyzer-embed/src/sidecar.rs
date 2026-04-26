//! On-disk binary format for embedding vectors.
//!
//! Vectors are stored in a sidecar file (`repo-intel.embeddings.bin`) next
//! to the JSON artifact so the JSON itself stays diffable and small. The
//! format is packed fp16 because:
//!
//! - 2 bytes per dim is small enough that a 1k-file repo at per-function
//!   × 256 dim is ~10 MB total.
//! - fp16 → fp32 conversion at load time is fast and the precision loss
//!   is well below cosine-similarity noise for retrieval-grade tasks.
//! - No quantization codebook to manage, no per-file scale/zero-point.
//!
//! Layout:
//!
//! ```text
//! header (json, length-prefixed):
//!   { "version": 1, "model_id": "...", "dim": 256, "vector_count": N }
//! body:
//!   for each vector:
//!     u32 le      path_len
//!     [u8]        path (utf-8)
//!     u8          chunk_kind (0=file, 1=function, 2=type, 3=doc_section)
//!     u32 le      start_line
//!     u32 le      end_line
//!     u32 le      name_len
//!     [u8]        name (utf-8, may be empty)
//!     [u16; dim]  values (fp16, little-endian)
//! ```
//!
//! Files referenced by the sidecar must also have an entry in the JSON
//! artifact's `embeddingsMeta.files` map keyed by path → content_hash
//! so `update` can detect changes without reading the sidecar.

use std::collections::HashMap;
use std::io::{Read, Write};

use anyhow::{Context, Result, anyhow, bail};
use half::f16;
use serde::{Deserialize, Serialize};

use crate::chunk::ChunkKind;

const SIDECAR_VERSION: u32 = 1;

// Caps on attacker-controlled length/count fields. A malformed or malicious
// sidecar could otherwise declare a 4 GiB header or 2^32 vectors and drive
// the reader into allocating attacker-chosen amounts of memory.
//
// Chosen to be comfortably above realistic usage:
//   - header is small JSON; 1 MiB is ~10x the largest plausible header.
//   - vector_count: BGE-small at per-function × very large monorepo
//     stays well under 10M.
//   - dim: BGE-large is 1024; 4096 covers foreseeable future models.
//   - name_len: paths/symbol names; 1 KiB is already generous.
const MAX_HEADER_LEN: usize = 1 << 20; // 1 MiB
const MAX_VECTOR_COUNT: usize = 10_000_000;
const MAX_DIM: usize = 4096;
const MAX_NAME_LEN: usize = 1024;
// Minimum bytes required per vector entry (path_len u32 + kind u8 +
// start u32 + end u32 + name_len u32 + fp16 values). Even with a zero-length
// path and zero-length name and dim=0 that's at least 17 bytes; we use 17
// as a conservative floor for the implied-size sanity check.
const MIN_BYTES_PER_VECTOR: usize = 17;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarHeader {
    pub version: u32,
    pub model_id: String,
    pub dim: usize,
    pub vector_count: usize,
}

/// In-memory representation of the sidecar. Read with [`Sidecar::read`],
/// modify, write back with [`Sidecar::write`].
#[derive(Debug, Clone)]
pub struct Sidecar {
    pub header: SidecarHeader,
    /// Keyed by repo-relative path, value is the list of vectors for that
    /// file in chunk order.
    pub vectors: HashMap<String, Vec<StoredVector>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredVector {
    pub kind: ChunkKind,
    pub name: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    /// fp16-stored values. Convert to f32 for similarity arithmetic via
    /// [`StoredVector::to_f32`].
    pub values: Vec<f16>,
}

impl StoredVector {
    pub fn to_f32(&self) -> Vec<f32> {
        self.values.iter().map(|v| v.to_f32()).collect()
    }

    pub fn from_f32(
        kind: ChunkKind,
        name: Option<String>,
        start_line: u32,
        end_line: u32,
        values: &[f32],
    ) -> Self {
        Self {
            kind,
            name,
            start_line,
            end_line,
            values: values.iter().map(|&v| f16::from_f32(v)).collect(),
        }
    }
}

impl Sidecar {
    /// Empty sidecar for a fresh scan.
    pub fn new(model_id: String, dim: usize) -> Self {
        Self {
            header: SidecarHeader {
                version: SIDECAR_VERSION,
                model_id,
                dim,
                vector_count: 0,
            },
            vectors: HashMap::new(),
        }
    }

    /// Drop the entry for one path. Used by `update` when a file is
    /// removed from the repo.
    pub fn remove(&mut self, path: &str) -> bool {
        if let Some(removed) = self.vectors.remove(path) {
            self.header.vector_count = self.header.vector_count.saturating_sub(removed.len());
            true
        } else {
            false
        }
    }

    /// Insert (or replace) all vectors for one path. Re-counts the total.
    pub fn insert(&mut self, path: String, vectors: Vec<StoredVector>) {
        if let Some(old) = self.vectors.remove(&path) {
            self.header.vector_count = self.header.vector_count.saturating_sub(old.len());
        }
        self.header.vector_count += vectors.len();
        self.vectors.insert(path, vectors);
    }

    /// Parse a sidecar from a byte slice. Prefer this over [`Sidecar::read`]
    /// when reading from an on-disk file because it additionally validates
    /// the declared vector count against the actual file size to bound
    /// up-front allocations.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        Self::read_inner(bytes, Some(bytes.len()))
    }

    /// Parse a sidecar from any `Read` source. Applies absolute caps on
    /// attacker-controlled fields but cannot cross-check against a total
    /// size - callers with an on-disk file should use [`Sidecar::from_bytes`].
    pub fn read<R: Read>(mut r: R) -> Result<Self> {
        // Slurp so we can apply the implied-size check consistently. The
        // caller already had to buffer the header/body to call read_exact
        // repeatedly, so this doesn't change peak memory by more than a
        // small constant in the happy path.
        let mut buf = Vec::new();
        r.read_to_end(&mut buf).context("read sidecar")?;
        Self::read_inner(&buf[..], Some(buf.len()))
    }

    fn read_inner(bytes: &[u8], total_len: Option<usize>) -> Result<Self> {
        let mut r = bytes;

        let header_len = read_u32(&mut r)? as usize;
        if header_len > MAX_HEADER_LEN {
            return Err(anyhow!(
                "sidecar header_len {} exceeds {} byte cap; file may be corrupt or malicious",
                header_len,
                MAX_HEADER_LEN
            ));
        }
        if let Some(total) = total_len
            && header_len.saturating_add(4) > total
        {
            return Err(anyhow!(
                "sidecar header_len {} exceeds available bytes {}",
                header_len,
                total
            ));
        }
        let mut header_buf = vec![0u8; header_len];
        r.read_exact(&mut header_buf).context("read header")?;
        let header: SidecarHeader =
            serde_json::from_slice(&header_buf).context("parse sidecar header")?;
        if header.version != SIDECAR_VERSION {
            bail!(
                "unsupported sidecar version {} (this binary supports {})",
                header.version,
                SIDECAR_VERSION
            );
        }
        if header.vector_count > MAX_VECTOR_COUNT {
            return Err(anyhow!(
                "sidecar vector_count {} exceeds {} cap; file may be corrupt or malicious",
                header.vector_count,
                MAX_VECTOR_COUNT
            ));
        }
        if header.dim > MAX_DIM {
            return Err(anyhow!(
                "sidecar dim {} exceeds {} cap; file may be corrupt or malicious",
                header.dim,
                MAX_DIM
            ));
        }
        // Cross-check declared count against actual bytes left. Each vector
        // entry requires at least MIN_BYTES_PER_VECTOR + 2*dim bytes.
        if let Some(total) = total_len {
            let per_vec = MIN_BYTES_PER_VECTOR.saturating_add(header.dim.saturating_mul(2));
            let min_body = header.vector_count.saturating_mul(per_vec);
            let available = total.saturating_sub(4 + header_len);
            if min_body > available {
                return Err(anyhow!(
                    "sidecar declares {} vectors (min {} bytes) but only {} body bytes remain",
                    header.vector_count,
                    min_body,
                    available
                ));
            }
        }

        let mut vectors: HashMap<String, Vec<StoredVector>> = HashMap::new();
        for _ in 0..header.vector_count {
            let path = read_string(&mut r)?;
            let kind_byte = read_u8(&mut r)?;
            let kind = decode_kind(kind_byte)?;
            let start_line = read_u32(&mut r)?;
            let end_line = read_u32(&mut r)?;
            let name_len = read_u32(&mut r)? as usize;
            if name_len > MAX_NAME_LEN {
                return Err(anyhow!(
                    "sidecar name_len {} exceeds {} byte cap; file may be corrupt or malicious",
                    name_len,
                    MAX_NAME_LEN
                ));
            }
            let name = if name_len == 0 {
                None
            } else {
                let mut buf = vec![0u8; name_len];
                r.read_exact(&mut buf).context("read name")?;
                Some(String::from_utf8(buf).context("name utf-8")?)
            };
            // header.dim is bounded by MAX_DIM above, so with_capacity is safe.
            let mut values = Vec::with_capacity(header.dim);
            for _ in 0..header.dim {
                values.push(f16::from_bits(read_u16(&mut r)?));
            }
            vectors.entry(path).or_default().push(StoredVector {
                kind,
                name,
                start_line,
                end_line,
                values,
            });
        }

        Ok(Self { header, vectors })
    }

    pub fn write<W: Write>(&self, mut w: W) -> Result<()> {
        let header_bytes = serde_json::to_vec(&self.header).context("serialize header")?;
        write_u32(&mut w, header_bytes.len() as u32)?;
        w.write_all(&header_bytes).context("write header")?;

        // Stable iteration: sort by path so the sidecar is reproducible.
        let mut paths: Vec<&String> = self.vectors.keys().collect();
        paths.sort();
        for path in paths {
            let vectors = &self.vectors[path];
            for v in vectors {
                write_string(&mut w, path)?;
                write_u8(&mut w, encode_kind(v.kind))?;
                write_u32(&mut w, v.start_line)?;
                write_u32(&mut w, v.end_line)?;
                let name_bytes = v.name.as_deref().unwrap_or("").as_bytes();
                write_u32(&mut w, name_bytes.len() as u32)?;
                w.write_all(name_bytes).context("write name")?;
                if v.values.len() != self.header.dim {
                    bail!(
                        "vector length {} does not match header dim {}",
                        v.values.len(),
                        self.header.dim
                    );
                }
                for value in &v.values {
                    write_u16(&mut w, value.to_bits())?;
                }
            }
        }
        Ok(())
    }
}

fn encode_kind(k: ChunkKind) -> u8 {
    match k {
        ChunkKind::File => 0,
        ChunkKind::Function => 1,
        ChunkKind::Type => 2,
        ChunkKind::DocSection => 3,
    }
}

fn decode_kind(b: u8) -> Result<ChunkKind> {
    Ok(match b {
        0 => ChunkKind::File,
        1 => ChunkKind::Function,
        2 => ChunkKind::Type,
        3 => ChunkKind::DocSection,
        _ => bail!("unknown chunk kind byte {b}"),
    })
}

fn read_u8<R: Read>(r: &mut R) -> Result<u8> {
    let mut b = [0u8; 1];
    r.read_exact(&mut b).context("read u8")?;
    Ok(b[0])
}

fn read_u16<R: Read>(r: &mut R) -> Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b).context("read u16")?;
    Ok(u16::from_le_bytes(b))
}

fn read_u32<R: Read>(r: &mut R) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b).context("read u32")?;
    Ok(u32::from_le_bytes(b))
}

fn read_string<R: Read>(r: &mut R) -> Result<String> {
    let len = read_u32(r)? as usize;
    if len > MAX_NAME_LEN {
        return Err(anyhow!(
            "sidecar string length {} exceeds {} byte cap; file may be corrupt or malicious",
            len,
            MAX_NAME_LEN
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).context("read string bytes")?;
    String::from_utf8(buf).context("string utf-8")
}

fn write_u8<W: Write>(w: &mut W, v: u8) -> Result<()> {
    w.write_all(&[v]).context("write u8")?;
    Ok(())
}

fn write_u16<W: Write>(w: &mut W, v: u16) -> Result<()> {
    w.write_all(&v.to_le_bytes()).context("write u16")?;
    Ok(())
}

fn write_u32<W: Write>(w: &mut W, v: u32) -> Result<()> {
    w.write_all(&v.to_le_bytes()).context("write u32")?;
    Ok(())
}

fn write_string<W: Write>(w: &mut W, s: &str) -> Result<()> {
    let bytes = s.as_bytes();
    write_u32(w, bytes.len() as u32)?;
    w.write_all(bytes).context("write string bytes")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_vector() -> StoredVector {
        StoredVector::from_f32(
            ChunkKind::Function,
            Some("alpha".into()),
            10,
            20,
            &[0.1, -0.2, 0.3, 0.0],
        )
    }

    #[test]
    fn empty_sidecar_round_trips() {
        let s = Sidecar::new("bge-small-en-v1.5-q8".into(), 4);
        let mut buf = Vec::new();
        s.write(&mut buf).unwrap();
        let back = Sidecar::read(&buf[..]).unwrap();
        assert_eq!(back.header.dim, 4);
        assert_eq!(back.header.vector_count, 0);
        assert!(back.vectors.is_empty());
    }

    #[test]
    fn populated_sidecar_round_trips() {
        let mut s = Sidecar::new("model-x".into(), 4);
        s.insert("src/a.rs".into(), vec![sample_vector(), sample_vector()]);
        s.insert("src/b.rs".into(), vec![sample_vector()]);

        let mut buf = Vec::new();
        s.write(&mut buf).unwrap();
        let back = Sidecar::read(&buf[..]).unwrap();
        assert_eq!(back.header.vector_count, 3);
        assert_eq!(back.vectors.len(), 2);
        assert_eq!(back.vectors["src/a.rs"].len(), 2);
        assert_eq!(back.vectors["src/a.rs"][0].name.as_deref(), Some("alpha"));
        let back_f32 = back.vectors["src/a.rs"][0].to_f32();
        // fp16 round-trip is approximate; verify within tolerance.
        let expected = [0.1f32, -0.2, 0.3, 0.0];
        for (got, want) in back_f32.iter().zip(expected.iter()) {
            assert!((got - want).abs() < 1e-3, "got {got}, want {want}");
        }
    }

    #[test]
    fn remove_drops_entry_and_decrements_count() {
        let mut s = Sidecar::new("m".into(), 4);
        s.insert("a.rs".into(), vec![sample_vector(), sample_vector()]);
        s.insert("b.rs".into(), vec![sample_vector()]);
        assert_eq!(s.header.vector_count, 3);
        assert!(s.remove("a.rs"));
        assert_eq!(s.header.vector_count, 1);
        assert!(!s.remove("a.rs"));
    }

    #[test]
    fn insert_replaces_existing_path() {
        let mut s = Sidecar::new("m".into(), 4);
        s.insert("a.rs".into(), vec![sample_vector(), sample_vector()]);
        assert_eq!(s.header.vector_count, 2);
        s.insert("a.rs".into(), vec![sample_vector()]);
        assert_eq!(s.header.vector_count, 1);
    }

    #[test]
    fn vector_length_mismatch_errors_on_write() {
        let mut s = Sidecar::new("m".into(), 4);
        let bad = StoredVector::from_f32(ChunkKind::File, None, 1, 1, &[0.1, 0.2]);
        s.insert("a.rs".into(), vec![bad]);
        let mut buf = Vec::new();
        let err = s.write(&mut buf).unwrap_err();
        assert!(err.to_string().contains("does not match header dim"));
    }

    /// Build a minimal sidecar byte stream by hand so we can inject
    /// attacker-controlled length fields without going through Sidecar::write.
    fn craft_header_only(header_json: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(header_json.len() as u32).to_le_bytes());
        buf.extend_from_slice(header_json);
        buf
    }

    #[test]
    fn oversized_header_len_is_rejected() {
        let mut buf = Vec::new();
        // Declare a 4 GiB - 1 header without supplying the bytes.
        buf.extend_from_slice(&(u32::MAX).to_le_bytes());
        let err = Sidecar::from_bytes(&buf[..]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("header_len") && msg.contains("cap"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn oversized_vector_count_is_rejected() {
        let header = br#"{"version":1,"model_id":"m","dim":4,"vector_count":99999999}"#;
        let buf = craft_header_only(header);
        let err = Sidecar::from_bytes(&buf[..]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("vector_count") && msg.contains("cap"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn oversized_dim_is_rejected() {
        let header = br#"{"version":1,"model_id":"m","dim":999999,"vector_count":0}"#;
        let buf = craft_header_only(header);
        let err = Sidecar::from_bytes(&buf[..]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("dim") && msg.contains("cap"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn oversized_name_len_is_rejected() {
        // Construct: header, then one vector entry with attacker name_len.
        // path_len=1, path='a', kind=0, start=0, end=0, name_len=MAX+1
        let header = br#"{"version":1,"model_id":"m","dim":0,"vector_count":1}"#;
        let mut buf = craft_header_only(header);
        buf.extend_from_slice(&1u32.to_le_bytes()); // path_len
        buf.push(b'a'); // path
        buf.push(0); // kind
        buf.extend_from_slice(&0u32.to_le_bytes()); // start
        buf.extend_from_slice(&0u32.to_le_bytes()); // end
        buf.extend_from_slice(&((MAX_NAME_LEN + 1) as u32).to_le_bytes()); // name_len
        // We don't add MAX_NAME_LEN+1 bytes - validation must reject before
        // attempting to allocate.
        let err = Sidecar::from_bytes(&buf[..]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("name_len") && msg.contains("cap"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn vector_count_mismatched_against_file_size_is_rejected() {
        // Declares 10M vectors but the body has 0 bytes; must fail fast.
        let header = br#"{"version":1,"model_id":"m","dim":4,"vector_count":1000000}"#;
        let buf = craft_header_only(header);
        let err = Sidecar::from_bytes(&buf[..]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("body bytes remain") || msg.contains("declares"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn write_is_deterministic_across_path_orderings() {
        let mut a = Sidecar::new("m".into(), 4);
        a.insert("z.rs".into(), vec![sample_vector()]);
        a.insert("a.rs".into(), vec![sample_vector()]);

        let mut b = Sidecar::new("m".into(), 4);
        b.insert("a.rs".into(), vec![sample_vector()]);
        b.insert("z.rs".into(), vec![sample_vector()]);

        let mut buf_a = Vec::new();
        let mut buf_b = Vec::new();
        a.write(&mut buf_a).unwrap();
        b.write(&mut buf_b).unwrap();
        assert_eq!(buf_a, buf_b);
    }
}
