use std::{path::PathBuf, sync::Arc};

use bytes::{Bytes, BytesMut};
use futures::TryStreamExt;
use mini_moka::sync::Cache;
use object_store::{ObjectStore, aws::AmazonS3Builder, local::LocalFileSystem, path::Path};
use serde::Deserialize;
use tokio::sync::{Mutex, Semaphore};

use crate::{AssetRef, Error, Index, Result};

const INDEX_LIMIT: usize = 64 * 1024 * 1024;
const IMAGE_LIMIT: usize = 64 * 1024 * 1024;

/// `prefix` is the full per-server root, e.g. `ixion/ui/sdo`.
/// A local directory may be used instead of S3 for offline use and tests.
#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct Config {
	pub endpoint: String,
	pub bucket: String,
	pub region: String,
	pub accesskey: String,
	pub secretkey: String,
	pub prefix: String,
	pub directory: Option<PathBuf>,
	pub cache: Option<PathBuf>,
	pub version: Option<String>,
}

impl Default for Config {
	fn default() -> Self {
		Self {
			endpoint: String::new(),
			bucket: String::new(),
			region: "us-east-1".into(),
			accesskey: String::new(),
			secretkey: String::new(),
			prefix: "ixion/ui/sdo".into(),
			directory: None,
			cache: None,
			version: None,
		}
	}
}

impl std::fmt::Debug for Config {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("AssetStorageConfig")
			.field("prefix", &self.prefix)
			.field("directory", &self.directory)
			.field("cache", &self.cache)
			.field("version", &self.version)
			.finish_non_exhaustive()
	}
}

#[derive(Clone, Debug)]
pub struct Snapshot {
	pub version: String,
	pub index: Arc<Index>,
}

pub struct Reader {
	store: Arc<dyn ObjectStore>,
	prefix: String,
	cache: Option<PathBuf>,
	current: String,
	indexes: Cache<String, Arc<Index>>,
	index_load: Mutex<()>,
	requests: Semaphore,
}

impl Reader {
	pub async fn new(config: Config) -> Result<Self> {
		let store: Arc<dyn ObjectStore> = if let Some(directory) = &config.directory {
			Arc::new(LocalFileSystem::new_with_prefix(directory).map_err(storage_error)?)
		} else {
			if config.endpoint.is_empty()
				|| config.bucket.is_empty()
				|| config.accesskey.is_empty()
				|| config.secretkey.is_empty()
			{
				return Err(Error::Invalid(
					"asset storage requires endpoint, bucket, accesskey and secretkey".into(),
				));
			}
			Arc::new(
				AmazonS3Builder::new()
					.with_endpoint(&config.endpoint)
					.with_bucket_name(&config.bucket)
					.with_region(&config.region)
					.with_access_key_id(&config.accesskey)
					.with_secret_access_key(&config.secretkey)
					.with_allow_http(config.endpoint.starts_with("http://"))
					.with_virtual_hosted_style_request(false)
					.build()
					.map_err(storage_error)?,
			)
		};
		Self::with_store(store, &config.prefix, config.cache, config.version).await
	}

	pub async fn with_store(
		store: Arc<dyn ObjectStore>,
		prefix: &str,
		cache: Option<PathBuf>,
		version: Option<String>,
	) -> Result<Self> {
		let prefix = prefix.trim_matches('/').to_owned();
		Path::parse(&prefix).map_err(|_| Error::Invalid("invalid storage prefix".into()))?;
		let mut reader = Self {
			store,
			prefix,
			cache,
			current: String::new(),
			indexes: Cache::new(4),
			index_load: Mutex::new(()),
			requests: Semaphore::new(32),
		};
		reader.current = if let Some(version) = version {
			version
		} else {
			#[derive(Deserialize)]
			struct Current {
				ffxiv: Option<String>,
				#[serde(rename = "lastValidIndex")]
				last_valid_index: Option<String>,
			}
			let bytes = reader.fetch("current.json", 64 * 1024).await?;
			let current: Current = serde_json::from_slice(&bytes)
				.map_err(|_| Error::Index("invalid current.json".into()))?;
			current
				.last_valid_index
				.filter(|v| !v.is_empty())
				.or(current.ffxiv)
				.ok_or_else(|| Error::Index("missing current version".into()))?
		};
		validate_version(&reader.current)?;
		reader.snapshot(None).await?;
		Ok(reader)
	}

	/// The default snapshot is pinned at startup, matching the old Go service.
	/// Explicit versions load their own index; a missing/corrupt index never
	/// silently falls back to a different version or to JSON.
	pub async fn snapshot(&self, requested: Option<&str>) -> Result<Snapshot> {
		let version = match requested {
			None | Some("latest") => &self.current,
			Some(v) => v,
		};
		validate_version(version)?;
		if let Some(index) = self.indexes.get(&version.to_owned()) {
			return Ok(Snapshot {
				version: version.into(),
				index,
			});
		}
		let _load = self.index_load.lock().await;
		if let Some(index) = self.indexes.get(&version.to_owned()) {
			return Ok(Snapshot {
				version: version.into(),
				index,
			});
		}
		let path = format!("patches/{version}/assets.bin");
		let bytes = match self.fetch(&path, INDEX_LIMIT).await {
			Err(Error::NotFound(_)) => {
				return Err(Error::Invalid(format!("unknown asset version {version}")));
			}
			other => other?,
		};
		let index = Arc::new(Index::parse(bytes)?);
		self.indexes.insert(version.into(), index.clone());
		Ok(Snapshot {
			version: version.into(),
			index,
		})
	}

	pub async fn read(&self, snapshot: &Snapshot, path: &str) -> Result<(AssetRef, Bytes)> {
		let entry = snapshot
			.index
			.lookup(path)?
			.ok_or_else(|| Error::NotFound(path.into()))?;
		let object = entry.object_path();
		if let Some(cache) = &self.cache {
			let path = cache.join(&object);
			match tokio::fs::metadata(&path).await {
				Ok(meta) if meta.len() > 0 && meta.len() <= IMAGE_LIMIT as u64 => {
					if let Ok(bytes) = tokio::fs::read(path).await {
						return Ok((entry, bytes.into()));
					}
				}
				_ => {}
			}
		}
		let bytes = self.fetch(&object, IMAGE_LIMIT).await?;
		if bytes.is_empty() {
			return Err(Error::Storage("empty image object".into()));
		}
		if let Some(cache) = &self.cache {
			let target = cache.join(&object);
			let temp = target.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
			let persist = async {
				tokio::fs::create_dir_all(target.parent().unwrap()).await?;
				tokio::fs::write(&temp, &bytes).await?;
				tokio::fs::rename(&temp, &target).await
			}
			.await;
			if persist.is_err() {
				let _ = tokio::fs::remove_file(&temp).await;
				tracing::warn!("could not persist asset cache entry");
			}
		}
		Ok((entry, bytes))
	}

	async fn fetch(&self, relative: &str, limit: usize) -> Result<Bytes> {
		let _permit = self
			.requests
			.acquire()
			.await
			.map_err(|_| Error::Storage("reader closed".into()))?;
		let key = if self.prefix.is_empty() {
			relative.into()
		} else {
			format!("{}/{relative}", self.prefix)
		};
		let path = Path::parse(key).map_err(|_| Error::Invalid("invalid object path".into()))?;
		let result = self.store.get(&path).await.map_err(|error| match error {
			object_store::Error::NotFound { .. } => Error::NotFound(relative.into()),
			other => storage_error(other),
		})?;
		if result.meta.size > limit {
			return Err(Error::Storage("object exceeds size limit".into()));
		}
		let mut stream = result.into_stream();
		let mut bytes = BytesMut::new();
		while let Some(chunk) = stream.try_next().await.map_err(storage_error)? {
			if chunk.len() > limit - bytes.len() {
				return Err(Error::Storage("object exceeds size limit".into()));
			}
			bytes.extend_from_slice(&chunk);
		}
		Ok(bytes.freeze())
	}
}

fn validate_version(value: &str) -> Result<()> {
	if value.is_empty()
		|| value.len() > 128
		|| value == "."
		|| value == ".."
		|| !value
			.bytes()
			.all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
	{
		return Err(Error::Invalid("invalid asset version".into()));
	}
	Ok(())
}

fn storage_error(error: object_store::Error) -> Error {
	// Do not expose endpoint URLs, credentials or signed request details to HTTP.
	tracing::debug!(kind = ?std::mem::discriminant(&error), "asset object-store request failed");
	Error::Storage("object-store request failed".into())
}

#[cfg(test)]
mod tests {
	use super::*;
	use object_store::memory::InMemory;

	fn golden() -> Bytes {
		let hex = format!(
			"4958415301004000000000002c00010001000000400000006c000000f9b87a16{}0ae203872dc9c845{}01000000",
			"00".repeat(32),
			"01".repeat(32)
		);
		(0..hex.len())
			.step_by(2)
			.map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
			.collect::<Vec<_>>()
			.into()
	}

	#[tokio::test]
	async fn reads_published_index_and_content_without_listing() {
		let store = Arc::new(InMemory::new());
		store
			.put(
				&Path::from("ui/sdo/current.json"),
				Bytes::from_static(br#"{"ffxiv":"other","lastValidIndex":"2026.09.15.0000.0000"}"#)
					.into(),
			)
			.await
			.unwrap();
		store
			.put(
				&Path::from("ui/sdo/patches/2026.09.15.0000.0000/assets.bin"),
				golden().into(),
			)
			.await
			.unwrap();
		let entry = Index::parse(golden())
			.unwrap()
			.lookup("ui/icon/000000/000000.tex")
			.unwrap()
			.unwrap();
		store
			.put(
				&Path::from(format!("ui/sdo/{}", entry.object_path())),
				Bytes::from_static(b"image bytes").into(),
			)
			.await
			.unwrap();
		let reader = Reader::with_store(store, "ui/sdo", None, None)
			.await
			.unwrap();
		let snapshot = reader.snapshot(Some("latest")).await.unwrap();
		assert_eq!(snapshot.version, "2026.09.15.0000.0000");
		assert_eq!(
			reader
				.read(&snapshot, "ui/icon/000000/000000.tex")
				.await
				.unwrap()
				.1,
			b"image bytes"[..]
		);
		assert!(matches!(
			reader.read(&snapshot, "ui/icon/000000/000001.tex").await,
			Err(Error::NotFound(_))
		));
		assert!(matches!(
			reader.snapshot(Some("unknown")).await,
			Err(Error::Invalid(_))
		));
		assert!(reader.snapshot(Some("../other")).await.is_err());
	}

	#[tokio::test]
	async fn corrupt_binary_does_not_fall_back_to_json() {
		let store = Arc::new(InMemory::new());
		store
			.put(
				&Path::from("patches/1/assets.bin"),
				Bytes::from_static(b"broken").into(),
			)
			.await
			.unwrap();
		store
			.put(
				&Path::from("patches/1/icons.json"),
				Bytes::from_static(b"[]").into(),
			)
			.await
			.unwrap();
		assert!(matches!(
			Reader::with_store(store, "", None, Some("1".into())).await,
			Err(Error::Index(_))
		));
	}
}
