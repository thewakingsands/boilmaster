use std::io::Cursor;

use anyhow::{Context, anyhow};
use bm_asset_index::Format;
use image::{DynamicImage, ImageFormat, Pixel, RgbaImage};

use crate::error::{Error, Result};

pub fn read(bytes: &[u8], format: Format) -> Result<DynamicImage> {
	let format = match format {
		Format::Webp => ImageFormat::WebP,
		Format::Avif => return read_avif(bytes),
		Format::Tex => {
			return Err(Error::UnsupportedSource(
				"tex".into(),
				"raw TEX objects are not encoded image assets".into(),
			));
		}
	};
	image::load_from_memory_with_format(bytes, format)
		.context("decode stored asset")
		.map_err(Into::into)
}

fn read_avif(bytes: &[u8]) -> Result<DynamicImage> {
	let decoded = avif_decode::Decoder::from_avif(bytes)
		.context("parse stored AVIF")?
		.to_image()
		.context("decode stored AVIF")?;
	// Respect each plane's stride and preserve alpha and high-bit-depth channels.
	macro_rules! convert {
		($image:expr, $variant:ident, $components:expr) => {{
			let (pixels, width, height) = $image.into_contiguous_buf();
			let width = u32::try_from(width).context("AVIF width exceeds u32")?;
			let height = u32::try_from(height).context("AVIF height exceeds u32")?;
			let data = pixels.into_iter().flat_map($components).collect();
			DynamicImage::$variant(
				image::ImageBuffer::from_raw(width, height, data)
					.ok_or_else(|| anyhow!("invalid AVIF pixel buffer"))?,
			)
		}};
	}
	Ok(match decoded {
		avif_decode::Image::Rgb8(image) => convert!(image, ImageRgb8, |p| [p.r, p.g, p.b]),
		avif_decode::Image::Rgb16(image) => convert!(image, ImageRgb16, |p| [p.r, p.g, p.b]),
		avif_decode::Image::Rgba8(image) => convert!(image, ImageRgba8, |p| [p.r, p.g, p.b, p.a]),
		avif_decode::Image::Rgba16(image) => convert!(image, ImageRgba16, |p| [p.r, p.g, p.b, p.a]),
		avif_decode::Image::Gray8(image) => convert!(image, ImageLuma8, |p| [p.value()]),
		avif_decode::Image::Gray16(image) => convert!(image, ImageLuma16, |p| [p.value()]),
	})
}

pub fn compose(mut foreground: RgbaImage, background: Option<RgbaImage>) -> Result<RgbaImage> {
	let Some(background) = background else {
		return Ok(foreground);
	};
	let (width, height) = background.dimensions();
	if width == 0 || height == 0 {
		return Err(anyhow!("empty map background").into());
	}
	if background
		.get_pixel(width / 2, height / 2)
		.channels()
		.iter()
		.all(|c| *c == 0)
	{
		return Ok(foreground);
	}
	if foreground.dimensions() != background.dimensions() {
		return Err(anyhow!("map and background dimensions differ").into());
	}
	for (target, source) in foreground.pixels_mut().zip(background.pixels()) {
		target.apply2(source, |a, b| ((u32::from(a) * u32::from(b)) / 255) as u8);
	}
	Ok(foreground)
}

pub fn write(image: impl Into<DynamicImage>, format: ImageFormat) -> Result<Vec<u8>> {
	let image = image.into();
	let image = if format == ImageFormat::Jpeg {
		DynamicImage::ImageRgb8(image.into_rgb8())
	} else if format == ImageFormat::WebP {
		// The WebP encoder accepts only 8-bit RGB/RGBA, unlike PNG.
		DynamicImage::ImageRgba8(image.into_rgba8())
	} else {
		image
	};
	let mut output = Cursor::new(Vec::new());
	image
		.write_to(&mut output, format)
		.context("encode asset response")?;
	Ok(output.into_inner())
}

#[cfg(test)]
mod tests {
	use super::*;
	use image::{ImageBuffer, Rgba};

	#[test]
	fn decodes_ixion_avif_with_and_without_alpha() {
		// Generated with ixion-tex formatTex(format=avif, quality=100, effort=1).
		// Synthetic 2x2 RGB(240,80,20), with alpha [0,64,128,255] or all 255.
		// ixion's ravif encoder writes 10-bit AVIF, exercising the u16 branches.
		for (hex, alpha) in [
			(include_str!("../tests/fixtures/rgb.avif.hex"), [255u8; 4]),
			(
				include_str!("../tests/fixtures/rgba.avif.hex"),
				[0, 64, 128, 255],
			),
		] {
			let hex = hex.trim();
			let bytes: Vec<_> = (0..hex.len())
				.step_by(2)
				.map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
				.collect();
			let decoded = read(&bytes, Format::Avif).unwrap();
			assert_eq!((decoded.width(), decoded.height()), (2, 2));
			assert_eq!(
				decoded.color().bits_per_pixel() / decoded.color().channel_count() as u16,
				16
			);
			for (pixel, alpha) in decoded.to_rgba8().pixels().zip(alpha) {
				assert!(
					pixel[3].abs_diff(alpha) <= 1,
					"{pixel:?}, expected alpha={alpha}"
				);
				if alpha > 0 {
					for (actual, expected) in pixel.0[..3].iter().zip([240, 80, 20]) {
						assert!(actual.abs_diff(expected) <= 3, "{pixel:?}");
					}
				}
			}
			for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP] {
				let output = write(decoded.clone(), format).unwrap();
				assert_eq!(image::guess_format(&output).unwrap(), format);
				assert_eq!(image::load_from_memory(&output).unwrap().width(), 2);
			}
		}
		assert!(read(b"broken avif", Format::Avif).is_err());
	}

	#[test]
	fn map_matches_upstream_multiply_and_precomposed_rules() {
		let foreground = ImageBuffer::from_pixel(2, 2, Rgba([200, 100, 50, 255]));
		let background = ImageBuffer::from_pixel(2, 2, Rgba([128, 255, 0, 128]));
		assert_eq!(
			compose(foreground.clone(), Some(background))
				.unwrap()
				.get_pixel(0, 0)
				.0,
			[100, 100, 0, 128]
		);
		assert_eq!(compose(foreground.clone(), None).unwrap(), foreground);
		let transparent = ImageBuffer::from_pixel(3, 3, Rgba([0, 0, 0, 0]));
		assert_eq!(
			compose(foreground.clone(), Some(transparent)).unwrap(),
			foreground
		);
		assert!(
			compose(
				foreground,
				Some(ImageBuffer::from_pixel(3, 3, Rgba([1, 1, 1, 1])))
			)
			.is_err()
		);
	}

	#[test]
	fn returns_real_png_jpeg_and_webp_bytes() {
		for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP] {
			let image = ImageBuffer::from_pixel(2, 2, Rgba([20u8, 40, 60, 128]));
			let bytes = write(image, format).unwrap();
			assert_eq!(image::guess_format(&bytes).unwrap(), format);
			assert_eq!(image::load_from_memory(&bytes).unwrap().width(), 2);
		}
	}
}
