//! Read-only IXAS v2 chunk lookup and storage, independent of HTTP and game EXD data.
mod index;
mod store;

pub use index::{AssetRef, Format, Index, normalize_path, path_hash};
pub use store::{Config, Reader, Snapshot};

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("invalid asset request: {0}")]
	Invalid(String),
	#[error("asset not found: {0}")]
	NotFound(String),
	#[error("invalid assets.bin: {0}")]
	Index(String),
	#[error("asset storage: {0}")]
	Storage(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
pub(crate) fn test_index() -> Vec<u8> {
	let hex = include_str!("../tests/fixtures/ixas-v2.hex").trim();
	(0..hex.len())
		.step_by(2)
		.map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
		.collect()
}
