//! Read-only IXAS v1 lookup and storage, independent of HTTP and game EXD data.
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
