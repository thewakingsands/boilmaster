use std::sync::{Arc, Mutex};

use bm_asset_index::{Reader, Snapshot, normalize_path};
use bytes::Bytes;
use mini_moka::sync::Cache;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio::time::{self, Duration, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::{Error, Format, error::Result, texture};

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
	pub enabled: bool,
	#[serde(flatten)]
	pub storage: bm_asset_index::Config,
}

#[cfg(test)]
mod tests {
	use super::*;
	use object_store::{ObjectStore, memory::InMemory, path::Path};

	#[tokio::test(start_paused = true)]
	async fn refreshes_hourly_recovers_after_failure_and_stops_on_shutdown() {
		let store = Arc::new(InMemory::new());
		let hex = format!(
			"4958415301004000000000002c00010001000000400000006c000000f9b87a16{}0ae203872dc9c845{}01000000",
			"00".repeat(32),
			"01".repeat(32)
		);
		let index: Bytes = (0..hex.len())
			.step_by(2)
			.map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
			.collect::<Vec<_>>()
			.into();
		for version in ["1", "2"] {
			store
				.put(
					&Path::from(format!("patches/{version}/assets.bin")),
					index.clone().into(),
				)
				.await
				.unwrap();
		}
		store
			.put(
				&Path::from("current.json"),
				Bytes::from_static(br#"{"ffxiv":"1"}"#).into(),
			)
			.await
			.unwrap();
		let reader = Arc::new(
			Reader::with_store(store.clone(), "", None, None)
				.await
				.unwrap(),
		);
		let service = Service::from_reader(Some(reader.clone()));
		let cancel = CancellationToken::new();
		let shutdown = cancel.clone();
		let task = tokio::spawn(async move { service.start(shutdown).await });
		tokio::task::yield_now().await;
		store
			.put(
				&Path::from("current.json"),
				Bytes::from_static(br#"{"ffxiv":"2"}"#).into(),
			)
			.await
			.unwrap();
		time::advance(Duration::from_secs(3599)).await;
		tokio::task::yield_now().await;
		assert_eq!(reader.snapshot(None).await.unwrap().version, "1");
		time::advance(Duration::from_secs(1)).await;
		tokio::task::yield_now().await;
		assert_eq!(reader.snapshot(None).await.unwrap().version, "2");
		assert!(reader.snapshots()[0].version == "2");

		store
			.put(
				&Path::from("current.json"),
				Bytes::from_static(b"broken").into(),
			)
			.await
			.unwrap();
		time::advance(Duration::from_secs(3600)).await;
		tokio::task::yield_now().await;
		assert_eq!(reader.snapshot(None).await.unwrap().version, "2");
		assert!(!task.is_finished());
		store
			.put(
				&Path::from("current.json"),
				Bytes::from_static(br#"{"ffxiv":"1"}"#).into(),
			)
			.await
			.unwrap();
		time::advance(Duration::from_secs(3600)).await;
		tokio::task::yield_now().await;
		assert_eq!(reader.snapshot(None).await.unwrap().version, "1");
		cancel.cancel();
		task.await.unwrap().unwrap();
	}

	#[tokio::test]
	async fn disabled_service_reports_reload_failure_and_releases_running_flag() {
		let service = Service::from_reader(None);
		assert!(!service.status().enabled);
		assert!(service.status().indexes.is_empty());
		assert!(service.reload().await.is_err());
		let status = service.status();
		assert!(!status.reload.running);
		assert!(status.reload.error.is_some());
		assert!(status.reload.checked_at.is_some());
	}
}

pub struct Service {
	reader: Option<Arc<Reader>>,
	workers: Arc<Semaphore>,
	encoded: Cache<String, Bytes>,
	reload_status: Mutex<ReloadStatus>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ReloadStatus {
	pub running: bool,
	pub checked_at: Option<u64>,
	pub changed: Option<bool>,
	pub error: Option<String>,
}

#[derive(Serialize)]
pub struct IndexStatus {
	pub version: String,
	pub fingerprint: String,
	pub entries: usize,
	pub current: bool,
}

#[derive(Serialize)]
pub struct Status {
	pub enabled: bool,
	pub pinned: bool,
	pub indexes: Vec<IndexStatus>,
	pub reload: ReloadStatus,
}

impl Service {
	pub fn status(&self) -> Status {
		Status {
			enabled: self.reader.is_some(),
			pinned: self.reader.as_ref().is_some_and(|reader| reader.pinned()),
			indexes: self
				.reader
				.as_ref()
				.map(|reader| {
					reader
						.snapshots()
						.into_iter()
						.enumerate()
						.map(|(i, snapshot)| IndexStatus {
							version: snapshot.version,
							fingerprint: snapshot.index.fingerprint().into(),
							entries: snapshot.index.len(),
							current: i == 0,
						})
						.collect()
				})
				.unwrap_or_default(),
			reload: self.reload_status.lock().expect("reload status").clone(),
		}
	}

	pub async fn reload(&self) -> Result<bool> {
		{
			let mut status = self.reload_status.lock().expect("reload status");
			if status.running {
				return Err(Error::Invalid("asset reload already running".into()));
			}
			status.running = true;
		}
		// Drop also clears running when shutdown cancels the in-flight refresh.
		struct Running<'a>(&'a Mutex<ReloadStatus>);
		impl Drop for Running<'_> {
			fn drop(&mut self) {
				self.0.lock().expect("reload status").running = false;
			}
		}
		let _running = Running(&self.reload_status);
		let result = match self.reader() {
			Ok(reader) => reader.reload().await.map_err(Error::from),
			Err(error) => Err(error),
		};
		let mut status = self.reload_status.lock().expect("reload status");
		status.checked_at = Some(
			std::time::SystemTime::now()
				.duration_since(std::time::UNIX_EPOCH)
				.unwrap_or_default()
				.as_secs(),
		);
		status.changed = result.as_ref().ok().copied();
		status.error = result.as_ref().err().map(ToString::to_string);
		result
	}

	/// Run hourly refreshes alongside the HTTP service, not on the request path.
	pub async fn start(&self, cancel: CancellationToken) -> Result<()> {
		let Some(_) = &self.reader else {
			return Ok(());
		};
		tokio::select! {
			_ = cancel.cancelled() => {},
			_ = async {
				let period = Duration::from_secs(3600);
				let mut interval = time::interval_at(time::Instant::now() + period, period);
				interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
				loop {
					interval.tick().await;
					if let Err(error) = self.reload().await {
						tracing::warn!(%error, "asset reload failed; retaining previous snapshot");
					}
				}
			} => {},
		}
		Ok(())
	}

	pub async fn new(config: Config) -> Result<Self> {
		let reader = if config.enabled {
			Some(Arc::new(Reader::new(config.storage).await?))
		} else {
			None
		};
		Ok(Self::from_reader(reader))
	}

	pub fn from_reader(reader: Option<Arc<Reader>>) -> Self {
		Self {
			reader,
			reload_status: Mutex::new(ReloadStatus::default()),
			workers: Arc::new(Semaphore::new(4)),
			encoded: Cache::builder()
				.max_capacity(64 * 1024 * 1024)
				.weigher(|_: &String, bytes: &Bytes| bytes.len().min(u32::MAX as usize) as u32)
				.build(),
		}
	}

	pub fn ready(&self) -> bool {
		true
	} // Enabled services load the initial index before startup.

	fn reader(&self) -> Result<&Reader> {
		self.reader.as_deref().ok_or(Error::Unavailable)
	}

	pub async fn snapshot(&self, version: Option<&str>) -> Result<Snapshot> {
		Ok(self.reader()?.snapshot(version).await?)
	}

	pub async fn raw(
		&self,
		snapshot: &Snapshot,
		path: &str,
	) -> Result<(bm_asset_index::AssetRef, Bytes)> {
		Ok(self.reader()?.read(snapshot, path).await?)
	}

	pub async fn convert(&self, snapshot: &Snapshot, path: &str, format: Format) -> Result<Bytes> {
		let path = normalize_path(path)?;
		if !path.ends_with(".tex") && !path.ends_with(".atex") {
			return Err(Error::InvalidConversion(path, format));
		}
		let key = format!(
			"{}:{path}:{}",
			snapshot.index.fingerprint(),
			format.extension()
		);
		if let Some(bytes) = self.encoded.get(&key) {
			return Ok(bytes);
		}
		let (source, bytes) = self.raw(snapshot, &path).await?;
		if matches!(
			(source.format, format),
			(bm_asset_index::Format::Webp, Format::Webp)
				| (bm_asset_index::Format::Avif, Format::Avif)
		) {
			return Ok(bytes);
		}
		if format == Format::Avif {
			return Err(Error::Invalid("format=avif only supports unchanged AVIF source objects; AVIF encoding is not supported".into()));
		}
		let permit = self
			.workers
			.clone()
			.acquire_owned()
			.await
			.map_err(|e| Error::Failure(e.into()))?;
		let output: Bytes = tokio::task::spawn_blocking(move || {
			let _permit = permit;
			texture::write(texture::read(&bytes, source.format)?, format.image_format())
		})
		.await
		.map_err(|e| Error::Failure(e.into()))??
		.into();
		self.encoded.insert(key, output.clone());
		Ok(output)
	}

	pub async fn map(
		&self,
		snapshot: &Snapshot,
		territory: &str,
		index: &str,
		format: Format,
	) -> Result<Bytes> {
		if format == Format::Avif {
			return Err(Error::Invalid("composed maps support only jpg, png and webp; use /api/asset with a source texture path for AVIF passthrough".into()));
		}
		let territory = territory.to_ascii_lowercase();
		if territory.is_empty()
			|| territory.len() > 64
			|| !territory
				.bytes()
				.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
			|| index.len() != 2
			|| !index.bytes().all(|b| b.is_ascii_digit())
		{
			return Err(Error::Invalid("invalid map territory or index".into()));
		}
		let key = format!(
			"{}:map:{territory}/{index}:{}",
			snapshot.index.fingerprint(),
			format.extension()
		);
		if let Some(bytes) = self.encoded.get(&key) {
			return Ok(bytes);
		}
		let stem = format!("ui/map/{territory}/{index}/{territory}{index}");
		let foreground = self.raw(snapshot, &format!("{stem}_m.tex")).await?;
		let background_path = format!("{stem}m_m.tex");
		let background = if snapshot.index.lookup(&background_path)?.is_some() {
			Some(self.raw(snapshot, &background_path).await?)
		} else {
			None
		};
		let permit = self
			.workers
			.clone()
			.acquire_owned()
			.await
			.map_err(|e| Error::Failure(e.into()))?;
		let output: Bytes = tokio::task::spawn_blocking(move || {
			let _permit = permit;
			let foreground = texture::read(&foreground.1, foreground.0.format)?.into_rgba8();
			let background = background
				.map(|(source, data)| texture::read(&data, source.format).map(|i| i.into_rgba8()))
				.transpose()?;
			let composed = texture::compose(foreground, background)?;
			texture::write(composed, format.image_format())
		})
		.await
		.map_err(|e| Error::Failure(e.into()))??
		.into();
		self.encoded.insert(key, output.clone());
		Ok(output)
	}
}
