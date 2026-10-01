//! Read-only real-data smoke check. Pipe a JSON bm_asset_index::Config to stdin.
//! Optional arguments: icon game path, map territory, map index.
use std::{io::Read, time::Instant};

use bm_asset::{Config, Format, Service};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
	let mut input = String::new();
	std::io::stdin().read_to_string(&mut input)?;
	let mut storage: bm_asset_index::Config = serde_json::from_str(&input)?;
	storage.cache = None;
	let service = Service::new(Config {
		enabled: true,
		storage,
	})
	.await?;
	let snapshot = service.snapshot(None).await?;
	println!(
		"version={} records={}",
		snapshot.version,
		snapshot.index.len()
	);
	let args: Vec<_> = std::env::args().skip(1).collect();
	let icon = args
		.first()
		.map(String::as_str)
		.unwrap_or("ui/icon/051000/051474_hr1.tex");
	let territory = args.get(1).map(String::as_str).unwrap_or("s1d1");
	let index = args.get(2).map(String::as_str).unwrap_or("00");
	for format in Format::iter() {
		if format == Format::Avif {
			let (source, stored) = service.raw(&snapshot, icon).await?;
			if source.format == bm_asset_index::Format::Avif {
				let output = service.convert(&snapshot, icon, format).await?;
				assert_eq!(output, stored);
				println!(
					"icon avif: byte-identical passthrough bytes={}",
					output.len()
				);
			}
			continue;
		}
		for map in [false, true] {
			let start = Instant::now();
			let bytes = if map {
				service.map(&snapshot, territory, index, format).await?
			} else {
				service.convert(&snapshot, icon, format).await?
			};
			let expected = match format {
				Format::Jpeg => image::ImageFormat::Jpeg,
				Format::Png => image::ImageFormat::Png,
				Format::Webp => image::ImageFormat::WebP,
				Format::Avif => image::ImageFormat::Avif,
			};
			assert_eq!(image::guess_format(&bytes)?, expected);
			let decoded = image::load_from_memory(&bytes)?;
			assert!(decoded.width() > 0 && decoded.height() > 0);
			println!(
				"{} {}: {}x{} bytes={} elapsed_ms={}",
				if map { "map" } else { "icon" },
				format.extension(),
				decoded.width(),
				decoded.height(),
				bytes.len(),
				start.elapsed().as_millis()
			);
		}
	}
	Ok(())
}
