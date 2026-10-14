//! Snapshots.
//!
//! Every stateful component implements [`Snapshottable`]. The VMM collects
//! the blobs into a [`Manifest`], writes guest RAM next to it, and hashes
//! everything so a corrupted or tampered snapshot is refused on restore.
#![forbid(unsafe_code)]

mod manifest;

pub use manifest::{Manifest, MemoryRange, SnapshotDir, FORMAT_VERSION};

use serde::{de::DeserializeOwned, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("encode: {0}")]
    Encode(String),
    #[error("decode: {0}")]
    Decode(String),
    #[error("snapshot is invalid: {0}")]
    Invalid(String),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A component whose state can be saved and restored.
///
/// `restore` input comes from disk: treat it as untrusted and validate
/// every field before using it.
pub trait Snapshottable {
    /// Stable id, unique within a VM (e.g. `"pl011@9000000"`).
    fn snapshot_id(&self) -> String;
    fn save(&mut self) -> Result<Vec<u8>>;
    fn restore(&mut self, data: &[u8]) -> Result<()>;

    /// Stop touching guest memory (background workers park). Called when
    /// the VM pauses and before `save`.
    fn pause(&mut self) {}
    fn resume(&mut self) {}
}

/// Serialize a state struct. JSON: small, debuggable, and serde is strict.
pub fn encode<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(v).map_err(|e| Error::Encode(e.to_string()))
}

pub fn decode<T: DeserializeOwned>(b: &[u8]) -> Result<T> {
    serde_json::from_slice(b).map_err(|e| Error::Decode(e.to_string()))
}
