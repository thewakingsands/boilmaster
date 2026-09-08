use std::{
	collections::HashMap,
	path::Path,
	sync::{Arc, RwLock},
};

use bm_version::VersionKey;
use figment::value::magic::RelativePathBuf;
use ironworks::{Ironworks, excel::Excel, sqpack::SqPack};
use tokio::sync::watch;

use crate::{
	error::{Error, Result},
	install::Install,
	release::{LocalVersion, UpdateStatus},
};

#[derive(Default)]
pub(crate) struct Snapshot {
	pub versions: Vec<LocalVersion>,
	pub loaded: HashMap<VersionKey, Arc<Version>>,
}

pub struct Data {
	pub(crate) channel: watch::Sender<Vec<VersionKey>>,
	pub(crate) snapshot: RwLock<Snapshot>,
	pub(crate) directory: std::path::PathBuf,
	pub(crate) update: std::sync::Mutex<UpdateStatus>,
	pub(crate) client: reqwest::Client,
	pub(crate) releases_url: String,
}

impl Data {
	pub fn new(directory: RelativePathBuf) -> Self {
		let (channel, _) = watch::channel(vec![]);
		Self {
			channel,
			snapshot: RwLock::new(Snapshot::default()),
			directory: directory.relative(),
			update: Default::default(),
			client: reqwest::Client::builder()
				.user_agent("boilmaster")
				.connect_timeout(std::time::Duration::from_secs(30))
				.timeout(std::time::Duration::from_secs(1800))
				.build()
				.expect("HTTP client"),
			releases_url: "https://api.github.com/repos/thewakingsands/ixion/releases/latest"
				.into(),
		}
	}

	pub fn ready(&self) -> bool {
		!self.snapshot.read().expect("poisoned").versions.is_empty()
	}

	pub fn subscribe(&self) -> watch::Receiver<Vec<VersionKey>> {
		self.channel.subscribe()
	}

	pub fn version_key(&self) -> VersionKey {
		self.snapshot.read().expect("poisoned").versions[0].internal_key()
	}

	pub fn version(&self, key: VersionKey) -> Result<Arc<Version>> {
		self.snapshot
			.read()
			.expect("poisoned")
			.loaded
			.get(&key)
			.cloned()
			.ok_or(Error::UnknownVersion(key))
	}

	pub fn versions(&self) -> Vec<LocalVersion> {
		self.snapshot.read().expect("poisoned").versions.clone()
	}

	pub fn public_key(&self, key: VersionKey) -> Result<String> {
		self.snapshot
			.read()
			.expect("poisoned")
			.versions
			.iter()
			.find(|v| v.internal_key() == key)
			.map(|v| v.key.clone())
			.ok_or(Error::UnknownVersion(key))
	}
}

pub struct Version {
	ironworks: Arc<Ironworks>,
	excel: Arc<Excel>,
}

impl Version {
	pub(crate) fn new(game_dir: &Path) -> anyhow::Result<Self> {
		let install = Install::at(game_dir);
		let ironworks = Arc::new(Ironworks::new().with_resource(SqPack::new(install)));
		let excel = Arc::new(Excel::new(ironworks.clone()));
		// Force a read: constructing Ironworks alone does not validate the archive.
		excel.list()?;
		Ok(Self { ironworks, excel })
	}

	pub fn ironworks(&self) -> Arc<Ironworks> {
		self.ironworks.clone()
	}
	pub fn excel(&self) -> Arc<Excel> {
		self.excel.clone()
	}
}
