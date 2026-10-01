use bytes::Bytes;
use sha2::{Digest, Sha256};

use crate::{Error, Result};

const HEADER: usize = 64;
const RECORD: usize = 44;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
	Webp,
	Avif,
	Tex,
}

impl Format {
	pub fn extension(self) -> &'static str {
		match self {
			Self::Webp => "webp",
			Self::Avif => "avif",
			Self::Tex => "tex",
		}
	}

	fn decode(value: u8) -> Result<Self> {
		match value {
			1 => Ok(Self::Webp),
			2 => Ok(Self::Avif),
			3 => Ok(Self::Tex),
			_ => Err(Error::Index("unsupported asset format".into())),
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssetRef {
	pub sha256: [u8; 32],
	pub format: Format,
}

impl AssetRef {
	pub fn object_path(self) -> String {
		let hash: String = self.sha256.iter().map(|v| format!("{v:02x}")).collect();
		format!("assets/{}/{hash}.{}", &hash[..2], self.format.extension())
	}
}

#[derive(Debug)]
pub struct Index {
	bytes: Bytes,
	count: usize,
	fingerprint: String,
}

impl Index {
	pub fn parse(bytes: Bytes) -> Result<Self> {
		let bad = |reason: &str| Error::Index(reason.into());
		if bytes.len() < HEADER {
			return Err(bad("truncated header"));
		}
		if &bytes[..4] != b"IXAS" {
			return Err(bad("magic"));
		}
		if u16_at(&bytes, 4) != 1 {
			return Err(bad("unsupported version"));
		}
		if u16_at(&bytes, 6) != HEADER as u16
			|| u32_at(&bytes, 8) != 0
			|| u16_at(&bytes, 12) != RECORD as u16
			|| u16_at(&bytes, 14) != 1
			|| u32_at(&bytes, 20) != HEADER as u32
			|| bytes[32..HEADER].iter().any(|b| *b != 0)
		{
			return Err(bad("unsupported header layout or flags"));
		}
		let count = u32_at(&bytes, 16) as usize;
		let expected = HEADER as u64 + count as u64 * RECORD as u64;
		if expected != bytes.len() as u64 || expected != u32_at(&bytes, 24) as u64 {
			return Err(bad("file size"));
		}
		if crc32fast::hash(&bytes[HEADER..]) != u32_at(&bytes, 28) {
			return Err(bad("checksum"));
		}
		let mut previous = None;
		for row in bytes[HEADER..].chunks_exact(RECORD) {
			let key = u64_at(row, 0);
			if previous.is_some_and(|p| p >= key) {
				return Err(bad("keys not strictly increasing"));
			}
			previous = Some(key);
			Format::decode(row[40])?;
			if row[41..].iter().any(|b| *b != 0) {
				return Err(bad("reserved record bytes"));
			}
		}
		let fingerprint = format!("{:x}", Sha256::digest(&bytes));
		Ok(Self {
			bytes,
			count,
			fingerprint,
		})
	}

	pub fn len(&self) -> usize {
		self.count
	}
	pub fn is_empty(&self) -> bool {
		self.count == 0
	}
	pub fn fingerprint(&self) -> &str {
		&self.fingerprint
	}

	pub fn lookup(&self, path: &str) -> Result<Option<AssetRef>> {
		let key = path_hash(path)?;
		let (mut low, mut high) = (0, self.count);
		while low < high {
			let mid = low + (high - low) / 2;
			let offset = HEADER + mid * RECORD;
			match u64_at(&self.bytes, offset).cmp(&key) {
				std::cmp::Ordering::Less => low = mid + 1,
				std::cmp::Ordering::Greater => high = mid,
				std::cmp::Ordering::Equal => {
					return Ok(Some(AssetRef {
						sha256: self.bytes[offset + 8..offset + 40].try_into().unwrap(),
						format: Format::decode(self.bytes[offset + 40])?,
					}));
				}
			}
		}
		Ok(None)
	}
}

pub fn normalize_path(path: &str) -> Result<String> {
	let path = path.to_ascii_lowercase();
	if path.len() >= 260
		|| !path.contains('/')
		|| !path
			.bytes()
			.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_./-".contains(&b))
		|| path
			.split('/')
			.any(|p| p.is_empty() || p == "." || p == "..")
	{
		return Err(Error::Invalid("malformed game path".into()));
	}
	Ok(path)
}

pub fn path_hash(path: &str) -> Result<u64> {
	let path = normalize_path(path)?;
	let (directory, file) = path.rsplit_once('/').unwrap();
	Ok((u64::from(!crc32fast::hash(directory.as_bytes())) << 32)
		| u64::from(!crc32fast::hash(file.as_bytes())))
}

fn u16_at(b: &[u8], at: usize) -> u16 {
	u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn u32_at(b: &[u8], at: usize) -> u32 {
	u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], at: usize) -> u64 {
	u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
	use super::*;

	fn golden() -> Vec<u8> {
		// The exact fixture produced by ixion's TypeScript encoder.
		let hex = format!(
			"4958415301004000000000002c00010001000000400000006c000000f9b87a16{}0ae203872dc9c845{}01000000",
			"00".repeat(32),
			"01".repeat(32)
		);
		(0..hex.len())
			.step_by(2)
			.map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
			.collect()
	}

	#[test]
	fn typescript_fixture() {
		assert_eq!(!crc32fast::hash(b"123456789"), 0x340bc6d9);
		let index = Index::parse(golden().into()).unwrap();
		assert_eq!(index.len(), 1);
		let entry = index.lookup("UI/ICON/000000/000000.TEX").unwrap().unwrap();
		assert_eq!(entry.sha256, [1; 32]);
		assert_eq!(entry.format, Format::Webp);
		assert_eq!(index.lookup("ui/icon/000000/000001.tex").unwrap(), None);
	}

	#[test]
	fn rejects_corrupt_files_and_paths() {
		for at in [0, 4, 6, 8, 12, 14, 16, 20, 24, 28, 32, 64, 80, 104, 105] {
			let mut bytes = golden();
			bytes[at] ^= 0xff;
			assert!(Index::parse(bytes.into()).is_err(), "offset {at}");
		}
		for length in 0..108 {
			assert!(Index::parse(golden()[..length].to_vec().into()).is_err());
		}
		for path in [
			"",
			"file.tex",
			"../file.tex",
			"ui//file.tex",
			"ui/../file.tex",
			"ui\\file.tex",
			"ui/é.tex",
		] {
			assert!(path_hash(path).is_err());
		}
	}

	#[test]
	fn validates_records_even_with_a_valid_checksum() {
		let checksum = |bytes: &mut Vec<u8>| {
			let value = crc32fast::hash(&bytes[HEADER..]);
			bytes[28..32].copy_from_slice(&value.to_le_bytes());
		};
		for offset in [104, 105, 106, 107] {
			let mut bytes = golden();
			bytes[offset] = 255;
			checksum(&mut bytes);
			assert!(Index::parse(bytes.into()).is_err());
		}
		for second_key in [path_hash("ui/icon/000000/000000.tex").unwrap(), 0] {
			let mut bytes = golden();
			let mut record = bytes[HEADER..].to_vec();
			record[..8].copy_from_slice(&second_key.to_le_bytes());
			bytes.extend_from_slice(&record);
			bytes[16..20].copy_from_slice(&2u32.to_le_bytes());
			bytes[24..28].copy_from_slice(&152u32.to_le_bytes());
			checksum(&mut bytes);
			assert!(Index::parse(bytes.into()).is_err());
		}
	}
}
