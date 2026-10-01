//! Read-only smoke check. Pipe a JSON Config to stdin; pass game paths as arguments.
//! No bucket listing or writes are performed (leave `cache` unset).
use std::io::Read;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
	let mut input = String::new();
	std::io::stdin().read_to_string(&mut input)?;
	let mut config: bm_asset_index::Config = serde_json::from_str(&input)?;
	config.cache = None;
	let reader = bm_asset_index::Reader::new(config).await?;
	let snapshot = reader.snapshot(None).await?;
	println!(
		"version={} records={} fingerprint={}",
		snapshot.version,
		snapshot.index.len(),
		snapshot.index.fingerprint()
	);
	for path in std::env::args().skip(1) {
		let (entry, bytes) = reader.read(&snapshot, &path).await?;
		println!(
			"{path}: format={} bytes={}",
			entry.format.extension(),
			bytes.len()
		);
	}
	Ok(())
}
