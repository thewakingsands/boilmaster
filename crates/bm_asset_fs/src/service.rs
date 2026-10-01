use std::sync::Arc;

use bm_asset_index::{Reader, Snapshot, normalize_path};
use bytes::Bytes;
use mini_moka::sync::Cache;
use serde::Deserialize;
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
}

pub struct Service {
	reader: Option<Arc<Reader>>,
	workers: Arc<Semaphore>,
	encoded: Cache<String, Bytes>,
}

impl Service {
	/// Run hourly refreshes alongside the HTTP service, not on the request path.
	pub async fn start(&self, cancel: CancellationToken) -> Result<()> {
		let Some(reader) = &self.reader else {
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
					if let Err(error) = reader.reload().await {
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
