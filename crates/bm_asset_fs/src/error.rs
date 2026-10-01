use super::format::Format;

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("invalid asset request: {0}")]
	Invalid(String),
	#[error("asset service is disabled")]
	Unavailable,
	#[error("source file \"{0}\" does not exist")]
	NotFound(String),

	#[error("source file \"{0}\" is unsupported: {1}")]
	UnsupportedSource(String, String),

	#[error("unknown format \"{0}\"")]
	UnknownFormat(String),

	#[error("{0} cannot be converted to {1:?}")]
	InvalidConversion(String, Format),

	#[error(transparent)]
	Failure(#[from] anyhow::Error),
}

impl From<bm_asset_index::Error> for Error {
	fn from(value: bm_asset_index::Error) -> Self {
		match value {
			bm_asset_index::Error::Invalid(message) => Self::Invalid(message),
			bm_asset_index::Error::NotFound(path) => Self::NotFound(path),
			other => Self::Failure(other.into()),
		}
	}
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
