use crate::{Error, Result};
use bytes::Bytes;
use sha2::{Digest, Sha256};
const HEADER: usize = 64;
const RECORD: usize = 20;
const CHUNK: usize = 24;
pub const IMAGE_LIMIT: u32 = 64 * 1024 * 1024;

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
	pub chunk: uuid::Uuid,
	pub chunk_size: u32,
	pub offset: u32,
	pub size: u32,
	pub format: Format,
}
impl AssetRef {
	pub fn object_path(self) -> String {
		format!("chunks/{}.bin", self.chunk)
	}
	pub fn cache_key(self) -> String {
		format!(
			"{}/{}-{}.{}",
			self.chunk,
			self.offset,
			self.size,
			self.format.extension()
		)
	}
}
#[derive(Debug)]
pub struct Index {
	bytes: Bytes,
	count: usize,
	records_at: usize,
	fingerprint: String,
}
impl Index {
	pub fn parse(bytes: Bytes) -> Result<Self> {
		let bad = |s: &str| Error::Index(s.into());
		if bytes.len() < HEADER {
			return Err(bad("truncated header"));
		}
		if &bytes[..4] != b"IXAS" || u16_at(&bytes, 4) != 2 {
			return Err(bad("unsupported format"));
		}
		if u16_at(&bytes, 6) != 64
			|| u32_at(&bytes, 8) != 0
			|| u16_at(&bytes, 12) != 20
			|| u16_at(&bytes, 14) != 1
			|| u32_at(&bytes, 36) != 64
			|| u16_at(&bytes, 40) != 24
			|| bytes[42..64].iter().any(|v| *v != 0)
		{
			return Err(bad("header"));
		}
		let count = u32_at(&bytes, 16) as usize;
		let chunks = u32_at(&bytes, 32) as usize;
		let records_at = u32_at(&bytes, 20) as usize;
		let expected = 64u64 + chunks as u64 * 24 + count as u64 * 20;
		if chunks > 65536
			|| records_at as u64 != 64 + chunks as u64 * 24
			|| expected != bytes.len() as u64
			|| expected != u32_at(&bytes, 24) as u64
		{
			return Err(bad("file size"));
		}
		if crc32fast::hash(&bytes[HEADER..]) != u32_at(&bytes, 28) {
			return Err(bad("checksum"));
		}
		let mut previous = None;
		for row in bytes[HEADER..records_at].chunks_exact(CHUNK) {
			let id: [u8; 16] = row[..16].try_into().unwrap();
			let size = u32_at(row, 16);
			if previous.is_some_and(|p| p >= id)
				|| size == 0 || size > IMAGE_LIMIT
				|| u32_at(row, 20) != 0
			{
				return Err(bad("chunk"));
			}
			previous = Some(id);
		}
		let mut previous = None;
		let mut used = vec![false; chunks];
		for row in bytes[records_at..].chunks_exact(RECORD) {
			let key = u64_at(row, 0);
			if previous.is_some_and(|p| p >= key) || row[19] != 0 {
				return Err(bad("record"));
			}
			previous = Some(key);
			Format::decode(row[18])?;
			let ci = u16_at(row, 16) as usize;
			let size = u32_at(row, 12);
			if ci >= chunks
				|| size == 0 || u64::from(u32_at(row, 8)) + u64::from(size)
				> u64::from(u32_at(&bytes, HEADER + ci * CHUNK + 16))
			{
				return Err(bad("range"));
			}
			used[ci] = true;
		}
		if used.iter().any(|v| !v) {
			return Err(bad("unreferenced chunk"));
		}
		let fingerprint = format!("{:x}", Sha256::digest(&bytes));
		Ok(Self {
			bytes,
			count,
			records_at,
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
			let at = self.records_at + mid * RECORD;
			match u64_at(&self.bytes, at).cmp(&key) {
				std::cmp::Ordering::Less => low = mid + 1,
				std::cmp::Ordering::Greater => high = mid,
				std::cmp::Ordering::Equal => {
					let ci = u16_at(&self.bytes, at + 16) as usize;
					let ca = HEADER + ci * CHUNK;
					return Ok(Some(AssetRef {
						chunk: uuid::Uuid::from_bytes(self.bytes[ca..ca + 16].try_into().unwrap()),
						chunk_size: u32_at(&self.bytes, ca + 16),
						offset: u32_at(&self.bytes, at + 8),
						size: u32_at(&self.bytes, at + 12),
						format: Format::decode(self.bytes[at + 18])?,
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
	#[test]
	fn full_u16_chunk_table_and_overflow() {
		let count = 65536usize;
		let records_at = 64 + count * 24;
		let mut b = vec![0u8; records_at + count * 20];
		b[..64].copy_from_slice(&crate::test_index()[..64]);
		b[16..20].copy_from_slice(&(count as u32).to_le_bytes());
		b[20..24].copy_from_slice(&(records_at as u32).to_le_bytes());
		let length = b.len() as u32;
		b[24..28].copy_from_slice(&length.to_le_bytes());
		b[32..36].copy_from_slice(&(count as u32).to_le_bytes());
		for i in 0..count {
			let ca = 64 + i * 24;
			let at = records_at + i * 20;
			b[ca..ca + 16].copy_from_slice(&(i as u128).to_be_bytes());
			b[ca + 16..ca + 20].copy_from_slice(&1u32.to_le_bytes());
			b[at..at + 8].copy_from_slice(&(i as u64).to_le_bytes());
			b[at + 12..at + 16].copy_from_slice(&1u32.to_le_bytes());
			b[at + 16..at + 18].copy_from_slice(&(i as u16).to_le_bytes());
			b[at + 18] = 1;
		}
		let crc = crc32fast::hash(&b[64..]);
		b[28..32].copy_from_slice(&crc.to_le_bytes());
		assert_eq!(Index::parse(b.clone().into()).unwrap().len(), 65536);
		b[32..36].copy_from_slice(&65537u32.to_le_bytes());
		assert!(Index::parse(b.into()).is_err());
	}
	#[test]
	fn shared_typescript_fixture() {
		let index = Index::parse(crate::test_index().into()).unwrap();
		let entry = index.lookup("UI/ICON/000000/000000.TEX").unwrap().unwrap();
		assert_eq!(entry.offset, 2);
		assert_eq!(entry.size, 11);
		assert_eq!(entry.chunk_size, 15);
		assert_eq!(entry.format, Format::Webp);
		assert_eq!(
			entry.object_path(),
			"chunks/01010101-0101-0101-0101-010101010101.bin"
		);
		assert!(index.lookup("ui/icon/000000/000001.tex").unwrap().is_none());
	}
	#[test]
	fn rejects_corruption_and_old_format() {
		let good = crate::test_index();
		for at in 0..good.len() {
			let mut b = good.clone();
			b[at] ^= 0xff;
			assert!(Index::parse(b.into()).is_err(), "offset {at}");
		}
		for length in 0..good.len() {
			assert!(Index::parse(good[..length].to_vec().into()).is_err());
		}
		for at in [64, 84, 96, 100, 104, 106, 107] {
			let mut b = good.clone();
			b[at] = 255;
			let crc = crc32fast::hash(&b[64..]);
			b[28..32].copy_from_slice(&crc.to_le_bytes());
			// UUID bytes are unconstrained, but range/flags/format must be checked.
			if at != 64 {
				assert!(Index::parse(b.into()).is_err(), "offset {at}");
			}
		}
		for path in [
			"",
			"file.tex",
			"../file.tex",
			"ui//file.tex",
			"ui/../file.tex",
			"ui/é.tex",
		] {
			assert!(path_hash(path).is_err());
		}
	}
}
