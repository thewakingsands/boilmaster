use std::str::FromStr;

use serde::{Deserialize, Serialize, de};
use strum::{EnumIter, IntoEnumIterator};

use super::error::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter)]
pub enum Format {
	Jpeg,
	Png,
	Webp,
	/// Return stored AVIF bytes unchanged; AVIF encoding is not supported.
	Avif,
}

impl Format {
	pub fn iter() -> impl Iterator<Item = Format> {
		<Self as IntoEnumIterator>::iter()
	}

	pub fn extension(&self) -> &str {
		match self {
			Self::Jpeg => "jpg",
			Self::Png => "png",
			Self::Webp => "webp",
			Self::Avif => "avif",
		}
	}

	pub(super) fn image_format(self) -> image::ImageFormat {
		match self {
			Self::Jpeg => image::ImageFormat::Jpeg,
			Self::Png => image::ImageFormat::Png,
			Self::Webp => image::ImageFormat::WebP,
			Self::Avif => image::ImageFormat::Avif,
		}
	}
}

// NOTE: Changing the string format is breaking to API1 - isolate if doing so.
impl Serialize for Format {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		self.extension().serialize(serializer)
	}
}

impl FromStr for Format {
	type Err = Error;

	fn from_str(input: &str) -> Result<Self, Self::Err> {
		Ok(match input {
			"jpg" => Self::Jpeg,
			"png" => Self::Png,
			"webp" => Self::Webp,
			"avif" => Self::Avif,
			other => return Err(Error::UnknownFormat(other.into())),
		})
	}
}

impl<'de> Deserialize<'de> for Format {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let raw = String::deserialize(deserializer)?;
		raw.parse().map_err(de::Error::custom)
	}
}
