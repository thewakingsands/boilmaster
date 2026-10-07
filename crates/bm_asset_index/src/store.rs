use std::{
	collections::HashMap,
	path::PathBuf,
	sync::{Arc, Mutex as StdMutex, RwLock, Weak},
};

use bytes::{Bytes, BytesMut};
use futures::TryStreamExt;
use mini_moka::sync::Cache;
use object_store::{
	GetOptions, GetRange, ObjectStore, aws::AmazonS3Builder, local::LocalFileSystem, path::Path,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
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
	// Initialized before the reader is exposed. Requests clone a whole snapshot
	// so a reload cannot mix a new version with an old index.
	current: RwLock<Option<Snapshot>>,
	pinned: bool,
	indexes: Cache<String, Arc<Index>>,
	index_load: Mutex<()>,
	requests: Semaphore,
	flights: StdMutex<HashMap<String, Weak<Mutex<Option<Bytes>>>>>,
}

impl Reader {
	pub fn pinned(&self) -> bool {
		self.pinned
	}

	/// Locally loaded indexes only; never enumerate the object store.
	pub fn snapshots(&self) -> Vec<Snapshot> {
		let current = self
			.current
			.read()
			.expect("asset snapshot lock poisoned")
			.as_ref()
			.expect("initialized")
			.clone();
		let mut snapshots: Vec<_> = self
			.indexes
			.iter()
			.filter(|entry| entry.key() != &current.version)
			.map(|entry| Snapshot {
				version: entry.key().clone(),
				index: entry.value().clone(),
			})
			.collect();
		snapshots.sort_by(|a, b| b.version.cmp(&a.version));
		snapshots.insert(0, current);
		snapshots
	}

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
		let cache = config.cache.map(|p| {
			p.join(format!(
				"{:x}",
				Sha256::digest(format!(
					"{}:{}:{}:{:?}",
					config.endpoint, config.bucket, config.prefix, config.directory
				))
			))
		});
		Self::with_store(store, &config.prefix, cache, config.version).await
	}

	pub async fn with_store(
		store: Arc<dyn ObjectStore>,
		prefix: &str,
		cache: Option<PathBuf>,
		version: Option<String>,
	) -> Result<Self> {
		let prefix = prefix.trim_matches('/').to_owned();
		Path::parse(&prefix).map_err(|_| Error::Invalid("invalid storage prefix".into()))?;
		let cache =
			cache.map(|p| p.join(format!("{:x}", Sha256::digest(format!("{store}:{prefix}")))));
		let reader = Self {
			store,
			prefix,
			cache,
			current: RwLock::new(None),
			pinned: version.is_some(),
			indexes: Cache::new(4),
			index_load: Mutex::new(()),
			requests: Semaphore::new(32),
			flights: StdMutex::new(HashMap::new()),
		};
		let version = if let Some(version) = version {
			version
		} else {
			reader.current_version().await?
		};
		let snapshot = reader.load_snapshot(&version).await?;
		reader.indexes.insert(version, snapshot.index.clone());
		*reader
			.current
			.write()
			.expect("asset snapshot lock poisoned") = Some(snapshot);
		Ok(reader)
	}

	async fn current_version(&self) -> Result<String> {
		#[derive(Deserialize)]
		struct Current {
			ffxiv: Option<String>,
			#[serde(rename = "lastValidIndex")]
			last_valid_index: Option<String>,
		}
		let bytes = self.fetch("current.json", 64 * 1024).await?;
		let current: Current = serde_json::from_slice(&bytes)
			.map_err(|_| Error::Index("invalid current.json".into()))?;
		current
			.last_valid_index
			.filter(|v| !v.is_empty())
			.or(current.ffxiv)
			.ok_or_else(|| Error::Index("missing current version".into()))
	}

	/// Reload both current.json and its index, including same-version replacements.
	/// A failed reload leaves the last valid snapshot and index cache untouched.
	/// Explicitly configured versions never follow current.json.
	pub async fn reload(&self) -> Result<bool> {
		if self.pinned {
			return Ok(false);
		}
		let _load = self.index_load.lock().await;
		let version = self.current_version().await?;
		let snapshot = self.load_snapshot(&version).await?;
		let mut current = self.current.write().expect("asset snapshot lock poisoned");
		let old = current.as_ref().expect("asset reader initialized");
		if old.version == snapshot.version
			&& old.index.fingerprint() == snapshot.index.fingerprint()
		{
			return Ok(false);
		}
		self.indexes.insert(version, snapshot.index.clone());
		tracing::info!(version = %snapshot.version, "asset snapshot reloaded");
		*current = Some(snapshot);
		Ok(true)
	}

	/// Default requests use the most recently validated snapshot.
	/// Explicit versions load their own index; a missing/corrupt index never
	/// silently falls back to a different version or to JSON.
	pub async fn snapshot(&self, requested: Option<&str>) -> Result<Snapshot> {
		let current = self
			.current
			.read()
			.expect("asset snapshot lock poisoned")
			.as_ref()
			.expect("asset reader initialized")
			.clone();
		let version = match requested {
			None | Some("latest") => return Ok(current),
			Some(v) if v == current.version => return Ok(current),
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
		let snapshot = self.load_snapshot(version).await?;
		self.indexes.insert(version.into(), snapshot.index.clone());
		Ok(snapshot)
	}

	async fn load_snapshot(&self, version: &str) -> Result<Snapshot> {
		validate_version(version)?;
		let path = format!("patches/{version}/assets.bin");
		let bytes = match self.fetch(&path, INDEX_LIMIT).await {
			Err(Error::NotFound(_)) => {
				return Err(Error::Invalid(format!("unknown asset version {version}")));
			}
			other => other?,
		};
		let index = Arc::new(Index::parse(bytes)?);
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
		let object = entry.cache_key();
		let flight = {
			let mut flights = self.flights.lock().expect("asset flights lock poisoned");
			flights.retain(|_, v| v.strong_count() > 0);
			if let Some(flight) = flights.get(&object).and_then(Weak::upgrade) {
				flight
			} else {
				let flight = Arc::new(Mutex::new(None));
				flights.insert(object.clone(), Arc::downgrade(&flight));
				flight
			}
		};
		let mut shared = flight.lock().await;
		if let Some(bytes) = shared.as_ref() {
			return Ok((entry, bytes.clone()));
		}
		if let Some(cache) = &self.cache {
			let path = cache.join(&object);
			match tokio::fs::metadata(&path).await {
				Ok(meta) if meta.len() == u64::from(entry.size) => {
					if let Ok(bytes) = tokio::fs::read(path).await {
						if bytes.len() == entry.size as usize {
							let bytes: Bytes = bytes.into();
							*shared = Some(bytes.clone());
							return Ok((entry, bytes));
						}
					}
				}
				_ => {}
			}
		}
		let bytes = self.fetch_range(entry).await?;
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
		*shared = Some(bytes.clone());
		Ok((entry, bytes))
	}

	async fn fetch_range(&self, entry: AssetRef) -> Result<Bytes> {
		let _permit = self
			.requests
			.acquire()
			.await
			.map_err(|_| Error::Storage("reader closed".into()))?;
		let relative = entry.object_path();
		let key = if self.prefix.is_empty() {
			relative.clone()
		} else {
			format!("{}/{}", self.prefix, relative)
		};
		let path = Path::parse(key).map_err(|_| Error::Invalid("invalid chunk path".into()))?;
		let range = entry.offset as usize..entry.offset as usize + entry.size as usize;
		let result = self
			.store
			.get_opts(
				&path,
				GetOptions {
					range: Some(GetRange::Bounded(range.clone())),
					..Default::default()
				},
			)
			.await
			.map_err(|error| match error {
				object_store::Error::NotFound { .. } => Error::NotFound(relative),
				other => storage_error(other),
			})?;
		if result.range != range || result.meta.size != entry.chunk_size as usize {
			return Err(Error::Storage("invalid chunk range or length".into()));
		}
		let mut stream = result.into_stream();
		let mut bytes = BytesMut::with_capacity(entry.size as usize);
		while let Some(part) = stream.try_next().await.map_err(storage_error)? {
			if part.len() > entry.size as usize - bytes.len() {
				return Err(Error::Storage("oversized chunk range".into()));
			}
			bytes.extend_from_slice(&part);
		}
		if bytes.len() != entry.size as usize || bytes.len() > IMAGE_LIMIT {
			return Err(Error::Storage("truncated chunk range".into()));
		}
		Ok(bytes.freeze())
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

	#[tokio::test]
	async fn coalesces_concurrent_ranges_and_rejects_truncated_chunks() {
		let store = Arc::new(InMemory::new());
		put(&store, "patches/1/assets.bin", golden()).await;
		let reader = Arc::new(
			Reader::with_store(store.clone(), "", None, Some("1".into()))
				.await
				.unwrap(),
		);
		let snapshot = reader.snapshot(None).await.unwrap();
		let path = "ui/icon/000000/000000.tex";
		let entry = snapshot.index.lookup(path).unwrap().unwrap();
		put(
			&store,
			&entry.object_path(),
			Bytes::from_static(b"xximage bytesy"),
		)
		.await;
		assert!(reader.read(&snapshot, path).await.is_err());
		put(
			&store,
			&entry.object_path(),
			Bytes::from_static(b"xximage bytesyy"),
		)
		.await;
		let permits = reader.requests.acquire_many(32).await.unwrap();
		let mut tasks = Vec::new();
		for _ in 0..3 {
			let reader = reader.clone();
			let snapshot = snapshot.clone();
			tasks.push(tokio::spawn(async move {
				reader.read(&snapshot, path).await.unwrap().1
			}));
		}
		for _ in 0..10 {
			tokio::task::yield_now().await;
		}
		assert!(
			reader
				.flights
				.lock()
				.unwrap()
				.get(&entry.cache_key())
				.unwrap()
				.strong_count()
				>= 3
		);
		drop(permits);
		for task in tasks {
			assert_eq!(task.await.unwrap(), b"image bytes"[..]);
		}
	}

	#[tokio::test]
	async fn disk_cache_uses_physical_uuid_not_snapshot_chunk_number() {
		let root = std::env::temp_dir().join(format!("bm-chunk-cache-{}", uuid::Uuid::new_v4()));
		let store = Arc::new(InMemory::new());
		put(&store, "patches/1/assets.bin", golden()).await;
		let reader = Reader::with_store(store.clone(), "", Some(root.clone()), Some("1".into()))
			.await
			.unwrap();
		let old = reader.snapshot(None).await.unwrap();
		let path = "ui/icon/000000/000000.tex";
		let entry = old.index.lookup(path).unwrap().unwrap();
		put(
			&store,
			&entry.object_path(),
			Bytes::from_static(b"xximage bytesyy"),
		)
		.await;
		assert_eq!(reader.read(&old, path).await.unwrap().1, b"image bytes"[..]);
		store
			.delete(&Path::from(entry.object_path()))
			.await
			.unwrap();
		let original = golden();
		let mut b = original[..64].to_vec();
		b.extend_from_slice(&[0u8; 16]);
		b.extend_from_slice(&1u32.to_le_bytes());
		b.extend_from_slice(&[0; 4]);
		b.extend_from_slice(&original[64..88]);
		b.extend_from_slice(&[0u8; 12]);
		b.extend_from_slice(&1u32.to_le_bytes());
		b.extend_from_slice(&[0, 0, 1, 0]);
		b.extend_from_slice(&original[88..]);
		b[16..20].copy_from_slice(&2u32.to_le_bytes());
		b[20..24].copy_from_slice(&112u32.to_le_bytes());
		b[24..28].copy_from_slice(&152u32.to_le_bytes());
		b[32..36].copy_from_slice(&2u32.to_le_bytes());
		b[148..150].copy_from_slice(&1u16.to_le_bytes());
		let crc = crc32fast::hash(&b[64..]);
		b[28..32].copy_from_slice(&crc.to_le_bytes());
		let new = Snapshot {
			version: "2".into(),
			index: Arc::new(Index::parse(b.into()).unwrap()),
		};
		assert_eq!(new.index.lookup(path).unwrap(), Some(entry));
		assert_eq!(reader.read(&new, path).await.unwrap().1, b"image bytes"[..]);
		tokio::fs::remove_dir_all(root).await.unwrap();
	}

	fn golden() -> Bytes {
		crate::test_index().into()
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
				Bytes::from_static(b"xximage bytesyy").into(),
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
	async fn reloads_versions_and_same_version_indexes_without_invalidating_snapshots() {
		let store = Arc::new(InMemory::new());
		put(
			&store,
			"current.json",
			Bytes::from_static(br#"{"ffxiv":"1"}"#),
		)
		.await;
		put(&store, "patches/1/assets.bin", golden()).await;
		let reader = Reader::with_store(store.clone(), "", None, None)
			.await
			.unwrap();
		let old = reader.snapshot(None).await.unwrap();
		assert!(!reader.reload().await.unwrap());

		let mut replacement = golden().to_vec();
		replacement[64..80].fill(2);
		let checksum = crc32fast::hash(&replacement[64..]);
		replacement[28..32].copy_from_slice(&checksum.to_le_bytes());
		put(&store, "patches/1/assets.bin", replacement.into()).await;
		assert!(reader.reload().await.unwrap());
		let updated = reader.snapshot(None).await.unwrap();
		assert_ne!(old.index.fingerprint(), updated.index.fingerprint());
		for requested in [None, Some("latest"), Some("1")] {
			assert_eq!(
				reader
					.snapshot(requested)
					.await
					.unwrap()
					.index
					.fingerprint(),
				updated.index.fingerprint()
			);
		}
		assert!(!reader.reload().await.unwrap());
		let path = "ui/icon/000000/000000.tex";
		for (snapshot, content) in [
			(&old, b"old content".as_slice()),
			(&updated, b"new content".as_slice()),
		] {
			let entry = snapshot.index.lookup(path).unwrap().unwrap();
			put(
				&store,
				&entry.object_path(),
				Bytes::from([b"xx".as_slice(), content, b"yy".as_slice()].concat()),
			)
			.await;
			assert_eq!(reader.read(snapshot, path).await.unwrap().1, content);
		}

		put(&store, "patches/2/assets.bin", golden()).await;
		put(
			&store,
			"current.json",
			Bytes::from_static(br#"{"ffxiv":"ignored","lastValidIndex":"2"}"#),
		)
		.await;
		assert!(reader.reload().await.unwrap());
		assert_eq!(reader.snapshot(None).await.unwrap().version, "2");
		assert_eq!(
			reader
				.snapshot(Some("1"))
				.await
				.unwrap()
				.index
				.fingerprint(),
			updated.index.fingerprint()
		);
		assert_eq!(reader.read(&old, path).await.unwrap().1, b"old content"[..]);
	}

	async fn put(store: &InMemory, path: &str, bytes: Bytes) {
		store.put(&Path::from(path), bytes.into()).await.unwrap();
	}

	#[tokio::test]
	async fn failed_reloads_retain_current_and_recover_on_retry() {
		let store = Arc::new(InMemory::new());
		put(
			&store,
			"current.json",
			Bytes::from_static(br#"{"ffxiv":"1","lastValidIndex":""}"#),
		)
		.await;
		put(&store, "patches/1/assets.bin", golden()).await;
		let reader = Reader::with_store(store.clone(), "", None, None)
			.await
			.unwrap();
		let old = reader.snapshot(None).await.unwrap();
		store.delete(&Path::from("current.json")).await.unwrap();
		assert!(reader.reload().await.is_err());
		for current in ["broken", "{}", r#"{"ffxiv":"../bad"}"#, r#"{"ffxiv":"2"}"#] {
			put(
				&store,
				"current.json",
				Bytes::copy_from_slice(current.as_bytes()),
			)
			.await;
			assert!(reader.reload().await.is_err());
			let active = reader.snapshot(None).await.unwrap();
			assert_eq!(active.version, "1");
			assert!(Arc::ptr_eq(&active.index, &old.index));
		}
		put(
			&store,
			"patches/2/assets.bin",
			Bytes::from_static(b"corrupt"),
		)
		.await;
		assert!(reader.reload().await.is_err());
		assert_eq!(reader.snapshot(None).await.unwrap().version, "1");
		put(&store, "patches/2/assets.bin", golden()).await;
		assert!(reader.reload().await.unwrap());
		assert_eq!(reader.snapshot(None).await.unwrap().version, "2");
	}

	#[tokio::test]
	async fn pinned_versions_never_reload_storage() {
		let store = Arc::new(InMemory::new());
		put(&store, "patches/1/assets.bin", golden()).await;
		let reader = Reader::with_store(store.clone(), "", None, Some("1".into()))
			.await
			.unwrap();
		store
			.delete(&Path::from("patches/1/assets.bin"))
			.await
			.unwrap();
		assert!(!reader.reload().await.unwrap());
		assert_eq!(reader.snapshot(None).await.unwrap().version, "1");
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
