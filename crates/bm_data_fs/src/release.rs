use std::{
	fs,
	path::Path,
	sync::Arc,
	time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail, ensure};
use bm_version::VersionKey;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::{Data, Version};

const KEEP_VERSIONS: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LocalVersion {
	pub key: String,
	pub version: String,
	pub published_at: String,
}

impl LocalVersion {
	// Keep the existing compact cache/search identifier internal. Public keys are release titles.
	pub(crate) fn internal_key(&self) -> VersionKey {
		format!("{:016x}", seahash::hash(self.key.as_bytes()))
			.parse()
			.expect("hash")
	}
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
	pub state: String,
	pub started_at: Option<String>,
	pub finished_at: Option<String>,
	pub updated: Option<bool>,
	pub error: Option<String>,
}

impl Default for UpdateStatus {
	fn default() -> Self {
		Self {
			state: "idle".into(),
			started_at: None,
			finished_at: None,
			updated: None,
			error: None,
		}
	}
}

#[derive(Deserialize)]
struct Release {
	name: String,
	published_at: String,
	draft: bool,
	prerelease: bool,
	assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
	name: String,
	browser_download_url: String,
	size: u64,
}

fn safe_key(key: &str) -> bool {
	!key.is_empty() && key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

fn asset_version(name: &str) -> Option<&str> {
	let version = name.strip_prefix("merged-")?.strip_suffix(".zip")?;
	(!version.is_empty() && version.bytes().all(|c| c.is_ascii_digit() || c == b'.'))
		.then_some(version)
}

fn timestamp() -> String {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.expect("clock")
		.as_nanos()
		.to_string()
}

impl Data {
	pub async fn initialize(self: &Arc<Self>) -> anyhow::Result<()> {
		let data = self.clone();
		tokio::task::spawn_blocking(move || data.load_local()).await??;
		self.begin_update();
		let result = self.clone().run_update().await;
		if let Err(error) = result {
			if !self.ready() {
				return Err(error);
			}
			tracing::warn!(?error, "release check failed; serving cached version");
		}
		Ok(())
	}

	pub fn update_status(&self) -> UpdateStatus {
		self.update.lock().expect("poisoned").clone()
	}

	fn begin_update(&self) -> UpdateStatus {
		let status = UpdateStatus {
			state: "running".into(),
			started_at: Some(timestamp()),
			..Default::default()
		};
		*self.update.lock().expect("poisoned") = status.clone();
		status
	}

	/// Concurrent triggers join the current job instead of starting another download.
	pub fn trigger_update(self: &Arc<Self>) -> UpdateStatus {
		let mut status = self.update.lock().expect("poisoned");
		if status.state == "running" {
			return status.clone();
		}
		*status = UpdateStatus {
			state: "running".into(),
			started_at: Some(timestamp()),
			..Default::default()
		};
		let response = status.clone();
		let data = self.clone();
		tokio::spawn(async move {
			let _ = data.run_update().await;
		});
		response
	}

	async fn run_update(self: Arc<Self>) -> anyhow::Result<()> {
		// Also turn worker panics into a terminal status, so callers never poll forever.
		let data = self.clone();
		let result = tokio::spawn(async move { data.download_latest().await })
			.await
			.context("update worker failed")
			.and_then(|r| r);
		let mut status = self.update.lock().expect("poisoned");
		status.finished_at = Some(timestamp());
		match &result {
			Ok(updated) => {
				status.state = "success".into();
				status.updated = Some(*updated);
			}
			Err(error) => {
				tracing::error!(?error, "release update failed");
				status.state = "error".into();
				status.error = Some(format!("{error:#}"));
			}
		}
		result.map(|_| ())
	}

	async fn download_latest(self: Arc<Self>) -> anyhow::Result<bool> {
		let release: Release = self
			.client
			.get(&self.releases_url)
			.send()
			.await?
			.error_for_status()?
			.json()
			.await?;
		ensure!(
			!release.draft && !release.prerelease,
			"latest release is not stable"
		);
		ensure!(safe_key(&release.name), "unsafe release title");
		let assets: Vec<_> = release
			.assets
			.iter()
			.filter(|asset| asset_version(&asset.name).is_some())
			.collect();
		ensure!(assets.len() == 1, "expected exactly one merged ZIP asset");
		let asset = assets[0];
		let metadata = LocalVersion {
			key: release.name,
			version: asset_version(&asset.name).unwrap().into(),
			published_at: release.published_at,
		};
		if self
			.versions()
			.first()
			.is_some_and(|v| v.key == metadata.key)
		{
			return Ok(false);
		}
		// Do not roll back if GitHub's latest designation points at an older release.
		if self
			.versions()
			.first()
			.is_some_and(|v| v.published_at > metadata.published_at)
		{
			return Ok(false);
		}
		let staging = tempfile::Builder::new()
			.prefix(".download-")
			.tempdir_in(&self.directory)?;
		let archive_path = staging.path().join("archive.zip");
		let mut file = tokio::fs::File::create(&archive_path).await?;
		let mut response = self
			.client
			.get(&asset.browser_download_url)
			.send()
			.await?
			.error_for_status()?;
		let mut downloaded = 0u64;
		while let Some(chunk) = response.chunk().await? {
			downloaded += chunk.len() as u64;
			ensure!(
				downloaded <= asset.size,
				"download exceeds release asset size"
			);
			file.write_all(&chunk).await?;
		}
		ensure!(downloaded == asset.size, "incomplete release asset");
		file.flush().await?;
		drop(file);
		let data = self.clone();
		tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
			let extracted = staging.path().join("game");
			extract_archive(&archive_path, &extracted, &metadata.version)?;
			Version::new(&extracted).context("invalid merged game data")?;
			fs::write(
				extracted.join("release.json"),
				serde_json::to_vec_pretty(&metadata)?,
			)?;
			let destination = data.directory.join(&metadata.key);
			ensure!(!destination.exists(), "release directory already exists");
			fs::rename(extracted, &destination)?;
			data.load_local()?;
			Ok(())
		})
		.await??;
		Ok(true)
	}

	fn load_local(&self) -> anyhow::Result<()> {
		fs::create_dir_all(&self.directory)?;
		let mut versions = Vec::new();
		let mut loaded = std::collections::HashMap::new();
		for entry in fs::read_dir(&self.directory)? {
			let entry = entry?;
			if !entry.file_type()?.is_dir() || !entry.path().join("release.json").is_file() {
				continue;
			}
			let metadata: LocalVersion =
				serde_json::from_slice(&fs::read(entry.path().join("release.json"))?)?;
			ensure!(
				safe_key(&metadata.key) && entry.file_name() == metadata.key.as_str(),
				"invalid local release key"
			);
			let version = Arc::new(Version::new(&entry.path())?);
			loaded.insert(metadata.internal_key(), version);
			versions.push(metadata);
		}
		versions.sort_by(|a, b| {
			b.published_at
				.cmp(&a.published_at)
				.then_with(|| b.key.cmp(&a.key))
		});
		// Only directories carrying validated release metadata are managed by retention.
		for version in versions.iter().skip(KEEP_VERSIONS) {
			fs::remove_dir_all(self.directory.join(&version.key))?;
			loaded.remove(&version.internal_key());
		}
		versions.truncate(KEEP_VERSIONS);
		let keys = versions
			.first()
			.map(|v| vec![v.internal_key()])
			.unwrap_or_default();
		*self.snapshot.write().expect("poisoned") = crate::data::Snapshot { versions, loaded };
		self.channel.send_if_modified(|current| {
			if *current == keys {
				false
			} else {
				*current = keys;
				true
			}
		});
		Ok(())
	}
}

fn extract_archive(
	archive: &Path,
	destination: &Path,
	expected_version: &str,
) -> anyhow::Result<()> {
	let mut archive = zip::ZipArchive::new(fs::File::open(archive)?)?;
	for i in 0..archive.len() {
		let mut entry = archive.by_index(i)?;
		let relative = entry.enclosed_name().context("unsafe ZIP path")?;
		if entry.is_symlink() {
			bail!("symlinks are not permitted in merged archives");
		}
		let path = destination.join(relative);
		if entry.is_dir() {
			fs::create_dir_all(&path)?;
			continue;
		}
		fs::create_dir_all(path.parent().context("missing parent")?)?;
		let mut output = fs::File::create(path)?;
		std::io::copy(&mut entry, &mut output)?;
	}
	ensure!(
		fs::read_to_string(destination.join("ffxivgame.ver"))?.trim() == expected_version,
		"archive version does not match filename"
	);
	for name in ["dat0", "index", "index2"] {
		ensure!(
			destination
				.join(format!("sqpack/ffxiv/0a0000.win32.{name}"))
				.is_file(),
			"missing sqpack {name}"
		);
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::Write;

	#[test]
	fn release_keys_and_asset_versions() {
		assert!(safe_key("20260908-6d044b4"));
		for key in ["", "../escape", "a/b", "a\\b", ".", "C:escape"] {
			assert!(!safe_key(key));
		}
		assert_eq!(
			asset_version("merged-2026.02.20.0000.0000.zip"),
			Some("2026.02.20.0000.0000")
		);
		for name in [
			"sdo-2026.zip",
			"merged-.zip",
			"merged-../escape.zip",
			"merged-2026.zip.exe",
		] {
			assert!(asset_version(name).is_none());
		}
	}

	#[test]
	fn extraction_rejects_traversal_and_wrong_version() {
		let directory = tempfile::tempdir().unwrap();
		for (name, contents) in [("../escape", "bad"), ("ffxivgame.ver", "wrong")] {
			let path = directory.path().join("test.zip");
			let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
			zip.start_file(name, zip::write::SimpleFileOptions::default())
				.unwrap();
			zip.write_all(contents.as_bytes()).unwrap();
			zip.finish().unwrap();
			assert!(
				extract_archive(
					&path,
					&directory.path().join("game"),
					"2026.02.20.0000.0000"
				)
				.is_err()
			);
			assert!(!directory.path().join("escape").exists());
		}
	}

	fn make_data(path: &Path, url: String) -> Arc<Data> {
		let mut data = Data::new(path.to_path_buf().into());
		data.releases_url = url;
		data.client = reqwest::Client::builder()
			.no_proxy()
			.timeout(std::time::Duration::from_secs(10))
			.build()
			.unwrap();
		Arc::new(data)
	}

	/// Real fixture integration test; set BM_TEST_ARCHIVE to a merged ZIP.
	#[tokio::test]
	#[ignore = "requires BM_TEST_ARCHIVE merged ZIP fixture"]
	async fn release_lifecycle() -> anyhow::Result<()> {
		use tokio::io::{AsyncReadExt, AsyncWriteExt};
		let archive_path = std::path::PathBuf::from(std::env::var("BM_TEST_ARCHIVE")?);
		let game_version = asset_version(archive_path.file_name().unwrap().to_str().unwrap())
			.unwrap()
			.to_owned();
		let archive = Arc::new(fs::read(archive_path)?);
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
		let root = format!("http://{}", listener.local_addr()?);
		let release = Arc::new(std::sync::RwLock::new(serde_json::json!({
			"name": "20260220-abcdef0", "published_at": "2026-02-20T00:00:00Z", "draft": false, "prerelease": false,
			"assets": [{"name": format!("merged-{game_version}.zip"), "browser_download_url": format!("{root}/archive"), "size": archive.len()}]
		})));
		let server_release = release.clone();
		let server = tokio::spawn(async move {
			loop {
				let (mut socket, _) = listener.accept().await.unwrap();
				let archive = archive.clone();
				let release = server_release.clone();
				tokio::spawn(async move {
					let mut request = vec![0; 8192];
					let size = socket.read(&mut request).await.unwrap();
					let is_archive =
						String::from_utf8_lossy(&request[..size]).starts_with("GET /archive ");
					let body = if is_archive {
						archive
					} else {
						Arc::new(serde_json::to_vec(&*release.read().unwrap()).unwrap())
					};
					socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
					let _ = socket.write_all(&body).await;
				});
			}
		});
		let directory = tempfile::tempdir()?;
		let data = make_data(directory.path(), format!("{root}/latest"));
		data.initialize().await?;
		assert!(data.ready());
		assert_eq!(data.versions()[0].key, "20260220-abcdef0");
		assert_eq!(data.versions()[0].version, game_version);
		assert_eq!(data.update_status().updated, Some(true));
		assert!(!data.clone().download_latest().await?);
		assert_eq!(data.subscribe().borrow().len(), 1);
		let old_key = data.version_key();

		// Publish another release with the same game version: release identity must still change.
		{
			let mut release = release.write().unwrap();
			release["name"] = "20260221-abcdef1".into();
			release["published_at"] = "2026-02-21T00:00:00Z".into();
		}
		let first = data.trigger_update();
		let second = data.trigger_update();
		assert_eq!(first.started_at, second.started_at);
		assert_eq!(first.state, "running");
		tokio::time::timeout(std::time::Duration::from_secs(60), async {
			while data.update_status().state == "running" {
				tokio::time::sleep(std::time::Duration::from_millis(20)).await;
			}
		})
		.await?;
		assert_eq!(data.update_status().state, "success");
		assert_ne!(old_key, data.version_key());
		assert_eq!(data.versions().len(), 2);

		// A corrupt/incomplete new release does not displace the current data.
		{
			let mut release = release.write().unwrap();
			release["name"] = "20260222-abcdef2".into();
			release["published_at"] = "2026-02-22T00:00:00Z".into();
			release["assets"][0]["size"] = 1.into();
		}
		assert!(data.clone().run_update().await.is_err());
		assert_eq!(data.update_status().state, "error");
		assert_eq!(data.versions()[0].key, "20260221-abcdef1");
		assert_eq!(fs::read_dir(directory.path())?.count(), 2);

		// Seed old releases with hard links to avoid duplicating the large fixture.
		let source = directory.path().join("20260220-abcdef0");
		for day in 1..=10 {
			let key = format!("202602{day:02}-aaaaaaa");
			let destination = directory.path().join(&key);
			fs::create_dir_all(destination.join("sqpack/ffxiv"))?;
			fs::hard_link(
				source.join("ffxivgame.ver"),
				destination.join("ffxivgame.ver"),
			)?;
			for entry in fs::read_dir(source.join("sqpack/ffxiv"))? {
				let entry = entry?;
				fs::hard_link(
					entry.path(),
					destination.join("sqpack/ffxiv").join(entry.file_name()),
				)?;
			}
			fs::write(
				destination.join("release.json"),
				serde_json::to_vec(&LocalVersion {
					key,
					version: game_version.clone(),
					published_at: format!("2026-02-{day:02}T00:00:00Z"),
				})?,
			)?;
		}
		data.load_local()?;
		assert_eq!(data.versions().len(), 10);
		assert_eq!(fs::read_dir(directory.path())?.count(), 10);
		assert!(!directory.path().join("20260201-aaaaaaa").exists());
		assert!(!directory.path().join("20260202-aaaaaaa").exists());
		assert_eq!(data.versions()[0].key, "20260221-abcdef1");
		server.abort();
		let restarted = make_data(directory.path(), format!("{root}/latest"));
		restarted.initialize().await?;
		assert_eq!(restarted.versions().len(), 10);
		assert_eq!(restarted.update_status().state, "error");
		assert_eq!(restarted.versions()[0].key, "20260221-abcdef1");
		Ok(())
	}
}
