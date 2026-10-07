//! Release-backed static documentation, installed without replacing the mount root.
use std::{
	fs,
	io::{Read, Write},
	path::{Component, Path, PathBuf},
	sync::{Arc, Mutex, RwLock},
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use axum::{
	body::Body,
	extract::{Request, State},
	http::StatusCode,
	response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use tower_http::services::ServeDir;

const INTERNAL: &str = ".boilmaster-docs";
const ARCHIVE_LIMIT: usize = 128 * 1024 * 1024;
const EXPANDED_LIMIT: u64 = 512 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Config {
	pub repository: String,
}
impl Default for Config {
	fn default() -> Self {
		Self {
			repository: "thewakingsands/xivapi-v2".into(),
		}
	}
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleaseInfo {
	pub directory: String,
	pub tag: String,
	pub published_at: String,
	pub sha256: String,
}

#[derive(Clone, Default, Serialize)]
pub struct Status {
	pub job_id: Option<String>,
	pub running: bool,
	pub checked_at: Option<u64>,
	pub changed: Option<bool>,
	pub error: Option<String>,
	pub current: Option<ReleaseInfo>,
}

pub struct Docs {
	root: PathBuf,
	config: Config,
	client: reqwest::Client,
	active: RwLock<Option<ReleaseInfo>>,
	status: Mutex<Status>,
}

#[derive(Deserialize)]
struct Release {
	tag_name: String,
	published_at: String,
	draft: bool,
	prerelease: bool,
	assets: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
	name: String,
	size: usize,
}

impl Docs {
	pub fn new(root: PathBuf, config: Config) -> Result<Arc<Self>> {
		let parts: Vec<_> = config.repository.split('/').collect();
		ensure!(
			parts.len() == 2
				&& parts.iter().all(|s| !s.is_empty()
					&& *s != "." && *s != ".."
					&& s.bytes()
						.all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))),
			"invalid documentation repository"
		);
		let active = match fs::read(root.join(INTERNAL).join("current.json")) {
			Ok(bytes) => {
				let info: ReleaseInfo =
					serde_json::from_slice(&bytes).context("invalid documentation current.json")?;
				ensure!(
					uuid::Uuid::parse_str(&info.directory).is_ok(),
					"invalid documentation directory"
				);
				validate_site(&root.join(INTERNAL).join(&info.directory).join("site"))?;
				Some(info)
			}
			Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
			Err(error) => return Err(error.into()),
		};
		Ok(Arc::new(Self {
			root,
			config,
			active: RwLock::new(active),
			status: Mutex::new(Status::default()),
			client: reqwest::Client::builder()
				.user_agent("boilmaster-docs")
				.https_only(true)
				.connect_timeout(Duration::from_secs(20))
				.timeout(Duration::from_secs(300))
				.build()?,
		}))
	}

	pub fn status(&self) -> Status {
		let mut status = self.status.lock().expect("docs status").clone();
		status.current = self.active.read().expect("docs snapshot").clone();
		status
	}

	pub fn trigger(self: &Arc<Self>) -> Status {
		let mut status = self.status.lock().expect("docs status");
		if !status.running {
			status.job_id = Some(uuid::Uuid::new_v4().to_string());
			status.running = true;
			status.checked_at = None;
			status.changed = None;
			status.error = None;
			let docs = self.clone();
			tokio::spawn(async move {
				let worker = docs.clone();
				let result = tokio::spawn(async move { worker.sync().await })
					.await
					.context("documentation worker failed")
					.and_then(|r| r);
				let mut status = docs.status.lock().expect("docs status");
				status.running = false;
				status.checked_at = Some(
					SystemTime::now()
						.duration_since(UNIX_EPOCH)
						.unwrap_or_default()
						.as_secs(),
				);
				status.changed = result.as_ref().ok().copied();
				status.error = result.as_ref().err().map(|e| format!("{e:#}"));
			});
		}
		drop(status);
		self.status()
	}

	async fn download(&self, url: reqwest::Url, limit: usize) -> Result<Vec<u8>> {
		let mut response = self.client.get(url).send().await?.error_for_status()?;
		ensure!(
			response
				.content_length()
				.is_none_or(|size| size <= limit as u64),
			"documentation response too large"
		);
		let mut bytes = Vec::new();
		while let Some(chunk) = response.chunk().await? {
			ensure!(
				chunk.len() <= limit - bytes.len(),
				"documentation download exceeds size limit"
			);
			bytes.extend_from_slice(&chunk);
		}
		Ok(bytes)
	}

	fn asset_url(&self, tag: &str, name: &str) -> reqwest::Url {
		let mut url = reqwest::Url::parse("https://github.com").expect("GitHub URL");
		url.path_segments_mut()
			.expect("base URL")
			.extend(self.config.repository.split('/'))
			.extend(["releases", "download", tag, name]);
		url
	}

	async fn sync(self: Arc<Self>) -> Result<bool> {
		let url = format!(
			"https://api.github.com/repos/{}/releases/latest",
			self.config.repository
		)
		.parse()?;
		let release: Release = serde_json::from_slice(&self.download(url, 1024 * 1024).await?)?;
		ensure!(
			!release.draft && !release.prerelease,
			"documentation release is not stable"
		);
		ensure!(
			!release.tag_name.is_empty() && !release.tag_name.contains(['/', '\\']),
			"invalid documentation tag"
		);
		let asset = release
			.assets
			.iter()
			.find(|asset| asset.name == "docs.zip")
			.context("release has no docs.zip")?;
		ensure!(
			asset.size > 0 && asset.size <= ARCHIVE_LIMIT,
			"documentation archive too large or empty"
		);
		ensure!(
			release
				.assets
				.iter()
				.any(|asset| asset.name == "docs.zip.sha256"),
			"release has no checksum"
		);
		let checksum = self
			.download(self.asset_url(&release.tag_name, "docs.zip.sha256"), 1024)
			.await?;
		let checksum = std::str::from_utf8(&checksum)?
			.split_whitespace()
			.next()
			.context("empty checksum")?
			.to_ascii_lowercase();
		ensure!(
			checksum.len() == 64 && checksum.bytes().all(|c| c.is_ascii_hexdigit()),
			"invalid checksum"
		);
		if self
			.active
			.read()
			.expect("docs snapshot")
			.as_ref()
			.is_some_and(|current| current.tag == release.tag_name && current.sha256 == checksum)
		{
			return Ok(false);
		}
		let bytes = self
			.download(self.asset_url(&release.tag_name, "docs.zip"), asset.size)
			.await?;
		ensure!(
			bytes.len() == asset.size,
			"incomplete documentation download"
		);
		ensure!(
			format!("{:x}", Sha256::digest(&bytes)) == checksum,
			"documentation checksum mismatch"
		);
		let docs = self.clone();
		tokio::task::spawn_blocking(move || {
			docs.install(bytes, &release.tag_name, &release.published_at, checksum)
		})
		.await??;
		Ok(true)
	}

	fn install(&self, bytes: Vec<u8>, tag: &str, published_at: &str, sha256: String) -> Result<()> {
		let managed = self.root.join(INTERNAL);
		fs::create_dir_all(&managed).context("documentation directory must be writable")?;
		let staging = tempfile::Builder::new()
			.prefix(".staging-")
			.tempdir_in(&managed)?;
		let site = staging.path().join("site");
		fs::create_dir(&site)?;
		extract(&bytes, &site)?;
		validate_site(&site)?;
		let info = ReleaseInfo {
			directory: uuid::Uuid::new_v4().to_string(),
			tag: tag.into(),
			published_at: published_at.into(),
			sha256,
		};
		fs::write(
			staging.path().join("release.json"),
			serde_json::to_vec_pretty(&info)?,
		)?;
		// Keep old directories intact, including for in-flight responses. The mount
		// itself is never renamed or deleted. Only our pointer file is replaced.
		fs::rename(staging.path(), managed.join(&info.directory))?;
		let mut pointer = tempfile::NamedTempFile::new_in(&managed)?;
		pointer.write_all(&serde_json::to_vec_pretty(&info)?)?;
		pointer.as_file().sync_all()?;
		pointer.persist(managed.join("current.json"))?;
		*self.active.write().expect("docs snapshot") = Some(info);
		Ok(())
	}

	fn directory(&self) -> PathBuf {
		self.active
			.read()
			.expect("docs snapshot")
			.as_ref()
			.map(|info| self.root.join(INTERNAL).join(&info.directory).join("site"))
			.unwrap_or_else(|| self.root.clone())
	}
}

fn validate_site(path: &Path) -> Result<()> {
	for file in ["index.html", "zh-cn/index.html", "en/index.html"] {
		ensure!(
			fs::metadata(path.join(file)).is_ok_and(|m| m.is_file() && m.len() > 0),
			"documentation archive missing {file}"
		);
	}
	Ok(())
}

fn extract(bytes: &[u8], target: &Path) -> Result<()> {
	let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
	ensure!(zip.len() <= 20_000, "too many documentation files");
	let mut total = 0u64;
	for i in 0..zip.len() {
		let mut entry = zip.by_index(i)?;
		let name = entry.name();
		ensure!(
			!name.contains(['\\', ':']) && !name.starts_with('/'),
			"unsafe archive path"
		);
		let path = PathBuf::from(name);
		ensure!(
			path.components().all(|c| matches!(c, Component::Normal(_)))
				&& !path.as_os_str().is_empty(),
			"unsafe archive path"
		);
		ensure!(
			path.components()
				.all(|c| !c.as_os_str().to_string_lossy().starts_with('.')),
			"hidden archive path"
		);
		ensure!(
			entry.unix_mode().is_none_or(|mode| mode & 0o170000 == 0
				|| mode & 0o170000 == 0o100000
				|| mode & 0o170000 == 0o040000),
			"archive contains a link or special file"
		);
		if entry.is_dir() {
			continue;
		}
		// The Astro docs-only API stubs must never act as live service endpoints.
		if path.starts_with("api")
			|| path.starts_with("admin")
			|| path.starts_with("health")
			|| path.starts_with("i")
		{
			continue;
		}
		total = total
			.checked_add(entry.size())
			.context("archive size overflow")?;
		ensure!(total <= EXPANDED_LIMIT, "expanded documentation too large");
		let output = target.join(path);
		fs::create_dir_all(output.parent().context("missing parent")?)?;
		let mut file = fs::OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(output)?;
		let expected = entry.size();
		let written = std::io::copy(&mut (&mut entry).take(expected + 1), &mut file)?;
		ensure!(written == expected, "invalid archive entry size");
	}
	Ok(())
}

pub async fn serve(State(docs): State<Arc<Docs>>, request: Request) -> Response {
	// Prevent direct reads of installation metadata/staging files in fallback mode.
	let decoded = percent_encoding::percent_decode_str(request.uri().path()).decode_utf8();
	if decoded.as_ref().map_or(true, |path| {
		path.split(['/', '\\'])
			.any(|part| part.eq_ignore_ascii_case(INTERNAL))
	}) {
		return StatusCode::NOT_FOUND.into_response();
	}
	match ServeDir::new(docs.directory()).oneshot(request).await {
		Ok(response) => response.map(Body::new),
		Err(never) => match never {},
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use axum::{Router, body::to_bytes, routing::get};
	use std::io::Cursor;

	fn archive(files: &[(&str, &str)]) -> Vec<u8> {
		let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
		for (path, contents) in files {
			zip.start_file(*path, zip::write::SimpleFileOptions::default())
				.unwrap();
			zip.write_all(contents.as_bytes()).unwrap();
		}
		zip.finish().unwrap().into_inner()
	}
	fn site(contents: &str) -> Vec<u8> {
		archive(&[
			("index.html", contents),
			("zh-cn/index.html", contents),
			("en/index.html", contents),
			("api/search", "fake"),
		])
	}

	#[tokio::test]
	async fn installs_switches_and_recovers_pointer_without_overwriting_mount() {
		let directory = tempfile::tempdir().unwrap();
		fs::write(directory.path().join("index.html"), "original mount").unwrap();
		let docs = Docs::new(directory.path().into(), Config::default()).unwrap();
		let app = Router::new().fallback(get(serve).with_state(docs.clone()));
		let response = app
			.clone()
			.oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
			.await
			.unwrap();
		assert_eq!(
			to_bytes(response.into_body(), 100).await.unwrap(),
			"original mount"
		);
		docs.install(site("first"), "one", "2026-10-04", "abc".into())
			.unwrap();
		let previous = docs.directory();
		assert_eq!(
			fs::read_to_string(previous.join("index.html")).unwrap(),
			"first"
		);
		assert!(!previous.join("api/search").exists());
		assert!(
			docs.install(
				archive(&[("index.html", "missing languages")]),
				"bad",
				"",
				"bad".into()
			)
			.is_err()
		);
		assert_eq!(docs.directory(), previous);
		docs.install(site("second"), "two", "2026-10-05", "def".into())
			.unwrap();
		assert!(previous.join("index.html").exists());
		assert_eq!(
			fs::read_to_string(directory.path().join("index.html")).unwrap(),
			"original mount"
		);
		let restarted = Docs::new(directory.path().into(), Config::default()).unwrap();
		assert_eq!(restarted.status().current.unwrap().tag, "two");
		assert_eq!(restarted.directory(), docs.directory());
		let response = app
			.clone()
			.oneshot(
				Request::builder()
					.uri("/zh-cn/")
					.body(Body::empty())
					.unwrap(),
			)
			.await
			.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		assert_eq!(to_bytes(response.into_body(), 100).await.unwrap(), "second");
		for path in [
			"/.boilmaster-docs/current.json",
			"/%2eboilmaster-docs/current.json",
		] {
			let response = app
				.clone()
				.oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
				.await
				.unwrap();
			assert_eq!(response.status(), StatusCode::NOT_FOUND);
		}
	}

	#[test]
	fn rejects_unsafe_archives_and_keeps_previous_site() {
		let directory = tempfile::tempdir().unwrap();
		let docs = Docs::new(directory.path().into(), Config::default()).unwrap();
		docs.install(site("safe"), "one", "", "abc".into()).unwrap();
		let original = docs.directory();
		for name in [
			"../outside.txt",
			"/absolute.txt",
			"C:/drive.txt",
			"en/../../outside.txt",
			"en\\outside.txt",
			".boilmaster-docs/current.json",
		] {
			assert!(
				docs.install(archive(&[(name, "bad")]), "bad", "", "bad".into())
					.is_err(),
				"{name}"
			);
			assert_eq!(docs.directory(), original);
		}
		assert!(
			docs.install(b"not a zip".to_vec(), "bad", "", "bad".into())
				.is_err()
		);
		let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
		zip.add_symlink(
			"link",
			"../../outside",
			zip::write::SimpleFileOptions::default(),
		)
		.unwrap();
		assert!(
			docs.install(zip.finish().unwrap().into_inner(), "bad", "", "bad".into())
				.is_err()
		);
		assert_eq!(
			fs::read_to_string(original.join("index.html")).unwrap(),
			"safe"
		);
	}

	#[test]
	fn rejects_repository_injection_and_unsafe_saved_pointer() {
		let directory = tempfile::tempdir().unwrap();
		for repository in [
			"../repo",
			"owner/repo?bad",
			"owner/repo/more",
			"https://evil.test",
		] {
			assert!(
				Docs::new(
					directory.path().into(),
					Config {
						repository: repository.into()
					}
				)
				.is_err()
			);
		}
		fs::create_dir(directory.path().join(INTERNAL)).unwrap();
		fs::write(
			directory.path().join(INTERNAL).join("current.json"),
			br#"{"directory":"../outside","tag":"bad","published_at":"","sha256":""}"#,
		)
		.unwrap();
		assert!(Docs::new(directory.path().into(), Config::default()).is_err());
	}

	#[tokio::test]
	async fn ci_update_api_authenticates_and_shares_admin_job() {
		let directory = tempfile::tempdir().unwrap();
		let docs = Docs::new(directory.path().into(), Config::default()).unwrap();
		docs.install(site("current"), "release-one", "", "abc".into())
			.unwrap();
		let app = crate::api1::docs::router(docs.clone(), Some("secret".into()));
		let request = |method: &str, uri: &str| {
			Request::builder()
				.method(method)
				.uri(uri)
				.body(Body::empty())
				.unwrap()
		};
		let response = app
			.clone()
			.oneshot(request("POST", "/docs/update"))
			.await
			.unwrap();
		assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
		assert_eq!(response.headers()["cache-control"], "no-store");
		assert!(docs.status().job_id.is_none());
		assert!(!docs.status().running);
		// Model a job started through the dashboard, without accessing GitHub.
		{
			let mut status = docs.status.lock().unwrap();
			status.running = true;
			status.job_id = Some("admin-job".into());
		}
		for method in ["GET", "POST", "POST"] {
			let uri = if method == "GET" {
				"/docs/update"
			} else {
				"/docs/update?token=secret"
			};
			let response = app.clone().oneshot(request(method, uri)).await.unwrap();
			assert_eq!(
				response.status(),
				if method == "GET" {
					StatusCode::OK
				} else {
					StatusCode::ACCEPTED
				}
			);
			assert_eq!(response.headers()["cache-control"], "no-store");
			let body: serde_json::Value =
				serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap())
					.unwrap();
			assert_eq!(body["running"], true);
			assert_eq!(body["job_id"], "admin-job");
			assert_eq!(body["current"]["tag"], "release-one");
		}
		{
			let mut status = docs.status.lock().unwrap();
			status.running = false;
			status.checked_at = Some(123);
			status.error = Some("download failed".into());
		}
		let response = app.oneshot(request("GET", "/docs/update")).await.unwrap();
		let body: serde_json::Value =
			serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
		assert_eq!(body["running"], false);
		assert_eq!(body["error"], "download failed");
		assert_eq!(body["current"]["tag"], "release-one");
	}

	#[tokio::test]
	async fn new_job_clears_previous_result_and_gets_an_identifier() {
		let directory = tempfile::tempdir().unwrap();
		let docs = Docs::new(directory.path().into(), Config::default()).unwrap();
		{
			let mut status = docs.status.lock().unwrap();
			status.job_id = Some("previous".into());
			status.checked_at = Some(123);
			status.changed = Some(true);
			status.error = Some("old error".into());
		}
		// No await: the current-thread runtime drops the spawned worker before any network I/O.
		let status = docs.trigger();
		assert!(status.running);
		assert!(uuid::Uuid::parse_str(status.job_id.as_deref().unwrap()).is_ok());
		assert!(status.checked_at.is_none());
		assert!(status.changed.is_none());
		assert!(status.error.is_none());
		assert_eq!(docs.trigger().job_id, status.job_id);
	}

	#[tokio::test]
	async fn repeated_sync_request_joins_existing_job() {
		let directory = tempfile::tempdir().unwrap();
		let docs = Docs::new(directory.path().into(), Config::default()).unwrap();
		docs.status.lock().unwrap().running = true;
		// No network request is spawned while the current job is running.
		assert!(docs.trigger().running);
		assert!(docs.trigger().running);
		assert!(docs.status().checked_at.is_none());
	}
}
