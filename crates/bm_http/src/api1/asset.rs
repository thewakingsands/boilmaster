use std::{
	ffi::OsStr,
	hash::{Hash, Hasher},
	time::Duration,
};

use aide::{
	axum::{ApiRouter, IntoApiResponse, routing::get_with},
	openapi,
	transform::TransformOperation,
};
use axum::{
	debug_handler,
	extract::{FromRef, FromRequestParts, OriginalUri, Request, State},
	http::{StatusCode, header, request::Parts},
	middleware,
	response::{IntoResponse, Response},
};
use axum_extra::{
	TypedHeader,
	headers::{CacheControl, ContentType, ETag, HeaderMapExt, IfNoneMatch},
};
use bm_asset::Format;
use schemars::{
	JsonSchema,
	r#gen::SchemaGenerator,
	schema::{InstanceType, Schema, SchemaObject},
};
use seahash::SeaHasher;
use serde::{Deserialize, Serialize};

use crate::service;

use super::{
	error::{Error, Result},
	extract::{Path, Query},
	jsonschema::impl_jsonschema,
};

// NOTE: Bump this if changing any behavior that impacts output binary data for assets, to ensure ETag is cache-broken.
const ASSET_ETAG_VERSION: usize = 5;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
	maxage: u64,
}

impl Default for Config {
	fn default() -> Self {
		Self { maxage: 604800 }
	}
}

#[derive(Clone, FromRef)]
struct AssetState {
	asset: service::Asset,
	config: Config,
}

pub fn router(config: Config, asset: service::Asset) -> ApiRouter {
	let state = AssetState { asset, config };

	ApiRouter::new()
		.api_route("/", get_with(asset2, asset2_docs))
		.api_route("/map/{territory}/{index}", get_with(map, map_docs))
		// Fall back to the old asset endpoint for compatibility.
		.route("/{*path}", axum::routing::get(asset1))
		.layer(middleware::from_fn_with_state(state.clone(), cache_layer))
		.with_state(state)
}

pub fn legacy_router(config: Config, asset: service::Asset) -> axum::Router {
	let state = AssetState { asset, config };
	axum::Router::new()
		.route("/{*path}", axum::routing::get(legacy_icon))
		.layer(middleware::from_fn_with_state(state.clone(), cache_layer))
		.layer(tower_http::cors::CorsLayer::permissive())
		.with_state(state)
}

#[derive(Deserialize, JsonSchema)]
struct VersionParams {
	/// Asset snapshot version, or `latest` (the snapshot selected at startup).
	version: Option<String>,
}

#[derive(aide::OperationIo)]
#[aide(input_with = "Query<VersionParams>")]
struct AssetVersion(bm_asset::Snapshot);

impl FromRequestParts<AssetState> for AssetVersion {
	type Rejection = Error;
	async fn from_request_parts(parts: &mut Parts, state: &AssetState) -> Result<Self> {
		if let Some(snapshot) = parts.extensions.get::<bm_asset::Snapshot>() {
			return Ok(Self(snapshot.clone()));
		}
		let Query(query) = Query::<VersionParams>::from_request_parts(parts, state).await?;
		let snapshot = state.asset.snapshot(query.version.as_deref()).await?;
		parts.extensions.insert(snapshot.clone());
		Ok(Self(snapshot))
	}
}

async fn legacy_icon(
	Path(Asset1Path { path }): Path<Asset1Path>,
	AssetVersion(snapshot): AssetVersion,
	State(asset): State<service::Asset>,
) -> Result<Response> {
	let (group, name) = path
		.split_once('/')
		.ok_or_else(|| Error::NotFound(path.clone()))?;
	let stem = name
		.strip_suffix(".png")
		.ok_or_else(|| Error::NotFound(path.clone()))?;
	let (id, hr) = stem
		.strip_suffix("_hr1")
		.map_or((stem, false), |id| (id, true));
	if group.len() != 6
		|| !group.bytes().all(|b| b.is_ascii_digit())
		|| id.len() != 6
		|| !id.bytes().all(|b| b.is_ascii_digit())
	{
		return Err(Error::NotFound(path));
	}
	let id: u32 = id.parse().map_err(|_| Error::NotFound(path.clone()))?;
	// The Go endpoint ignored the supplied group and resolved by ID + HR.
	let game_path = format!(
		"ui/icon/{:06}/{id:06}{}.tex",
		id / 1000 * 1000,
		if hr { "_hr1" } else { "" }
	);
	let (source, bytes) = asset.raw(&snapshot, &game_path).await?;
	Ok((TypedHeader(ContentType::from(stored_mime(source))), bytes).into_response())
}

// Original asset endpoint based on a game path in the url path.

#[derive(Deserialize)]
struct Asset1Path {
	path: String,
}

#[derive(Deserialize)]
struct Asset1Query {
	format: Option<SchemaFormat>,
}

#[debug_handler(state = AssetState)]
async fn asset1(
	Path(Asset1Path { path }): Path<Asset1Path>,
	query_version: AssetVersion,
	Query(Asset1Query { format }): Query<Asset1Query>,
	state_service: State<service::Asset>,
) -> Result<impl IntoApiResponse> {
	// The endpoints are nearly identical - just call through to the new endpoint with an emulated query.
	asset2(
		query_version,
		Query(AssetQuery { path, format }),
		state_service,
	)
	.await
}

/// Query parameters accepted by the asset endpoint.
#[derive(Deserialize, JsonSchema)]
struct AssetQuery {
	/// Game path of the asset to retrieve.
	#[schemars(example = "example_path")]
	path: String,

	/// Optional output format. Omit to return the stored object unchanged.
	/// Explicit `avif` is available only when the source is already AVIF.
	#[schemars(example = "example_format")]
	format: Option<SchemaFormat>,
}

fn example_path() -> &'static str {
	"ui/icon/051000/051474_hr1.tex"
}

#[derive(Serialize, Deserialize)]
#[repr(transparent)]
struct SchemaFormat(Format);

impl_jsonschema!(SchemaFormat, format_schema);
fn format_schema(_generator: &mut SchemaGenerator) -> Schema {
	formats_schema(Format::iter())
}

fn map_format_schema(_generator: &mut SchemaGenerator) -> Schema {
	formats_schema(Format::iter().filter(|format| *format != Format::Avif))
}

fn formats_schema(formats: impl Iterator<Item = Format>) -> Schema {
	Schema::Object(SchemaObject {
		instance_type: Some(InstanceType::String.into()),
		enum_values: Some(
			formats
				.map(|format| serde_json::to_value(format).expect("should not fail"))
				.collect(),
		),
		..Default::default()
	})
}

fn example_format() -> SchemaFormat {
	SchemaFormat(Format::Png)
}

fn asset2_docs(operation: TransformOperation) -> TransformOperation {
	operation
		.summary("read an asset")
		.description("Read an indexed asset at the specified game path. Omit format to return the stored object byte-for-byte with its original content type. Explicit jpg, png and webp select an output encoding; avif returns an existing AVIF object unchanged. Non-AVIF sources cannot be requested as avif (400).")
		.response_with::<200, Vec<u8>, _>(|mut response| {
			response.inner().content = Format::iter()
				.map(|format| {
					(
						format_mime(format).to_string(),
						openapi::MediaType::default(),
					)
				})
				.collect();
			response
		})
		.response_with::<304, (), _>(|res| res.description("not modified"))
}

#[debug_handler(state = AssetState)]
async fn asset2(
	AssetVersion(snapshot): AssetVersion,
	Query(AssetQuery { path, format }): Query<AssetQuery>,
	State(asset): State<service::Asset>,
) -> Result<impl IntoApiResponse> {
	let (bytes, content_type, extension) = match format {
		Some(SchemaFormat(format)) => (
			asset.convert(&snapshot, &path, format).await?,
			format_mime(format),
			format.extension().to_owned(),
		),
		None => {
			let (source, bytes) = asset.raw(&snapshot, &path).await?;
			(
				bytes,
				stored_mime(source),
				source.format.extension().to_owned(),
			)
		}
	};

	// Try to derive a filename to use for the Content-Disposition header.
	let filepath = std::path::Path::new(&path).with_extension(extension);
	let disposition = match filepath.file_name().and_then(OsStr::to_str) {
		Some(name) => format!("inline; filename=\"{name}\""),
		None => "inline".to_string(),
	};

	let response = (
		TypedHeader(ContentType::from(content_type)),
		// TypedHeader only has a really naive inline value with no ability to customise :/
		[(header::CONTENT_DISPOSITION, disposition)],
		bytes,
	);

	Ok(response.into_response())
}

fn format_mime(format: Format) -> mime::Mime {
	match format {
		Format::Jpeg => mime::IMAGE_JPEG,
		Format::Png => mime::IMAGE_PNG,
		Format::Webp => "image/webp".parse().expect("mime parse should not fail"),
		Format::Avif => "image/avif".parse().expect("mime parse should not fail"),
	}
}

fn stored_mime(source: bm_asset::AssetRef) -> mime::Mime {
	match source.format.extension() {
		"webp" => format_mime(Format::Webp),
		"avif" => format_mime(Format::Avif),
		_ => mime::APPLICATION_OCTET_STREAM,
	}
}

/// Path segments expected by the asset map endpoint.
#[derive(Debug, Deserialize, JsonSchema)]
struct MapPath {
	/// Territory of the map to be retrieved. This typically takes the form of 4
	/// characters, [letter][number][letter][number]. See `Map`'s `Id` field for
	/// examples of possible combinations of `territory` and `index`.
	#[schemars(example = "example_territory")]
	territory: String,

	/// Index of the map within the territory. This invariably takes the form of a
	/// two-digit zero-padded number. See `Map`'s `Id` field for examples of
	/// possible combinations of `territory` and `index`.
	#[schemars(example = "example_index")]
	index: String,
}

fn example_territory() -> &'static str {
	"s1d1"
}

fn example_index() -> &'static str {
	"00"
}

#[derive(Deserialize, JsonSchema)]
struct MapQuery {
	#[serde(default = "default_map_format")]
	#[schemars(schema_with = "map_format_schema")]
	format: SchemaFormat,
}

fn default_map_format() -> SchemaFormat {
	SchemaFormat(Format::Jpeg)
}

fn map_docs(operation: TransformOperation) -> TransformOperation {
	operation
		.summary("compose a map")
		.description(
			"Retrieve the specified map, composing it from split source files if necessary. The format defaults to jpg; png and webp are also supported. AVIF passthrough is available only through /asset with a source texture path, not this composition endpoint.",
		)
		.response_with::<200, Vec<u8>, _>(|mut response| {
			response.inner().content = Format::iter()
				.filter(|format| *format != Format::Avif)
				.map(|format| {
					(
						format_mime(format).to_string(),
						openapi::MediaType::default(),
					)
				})
				.collect();
			response
		})
		.response_with::<304, (), _>(|res| res.description("not modified"))
}

#[debug_handler(state = AssetState)]
async fn map(
	Path(MapPath { territory, index }): Path<MapPath>,
	AssetVersion(snapshot): AssetVersion,
	Query(MapQuery {
		format: SchemaFormat(format),
	}): Query<MapQuery>,
	State(asset): State<service::Asset>,
) -> Result<impl IntoApiResponse> {
	let bytes = asset.map(&snapshot, &territory, &index, format).await?;

	let response = (
		TypedHeader(ContentType::from(format_mime(format))),
		[(
			header::CONTENT_DISPOSITION,
			format!(
				"inline; filename=\"{territory}_{index}.{extension}\"",
				extension = format.extension()
			),
		)],
		bytes,
	);

	Ok(response.into_response())
}

async fn cache_layer(
	uri: OriginalUri,
	AssetVersion(snapshot): AssetVersion,
	header_if_none_match: Option<TypedHeader<IfNoneMatch>>,
	State(config): State<Config>,
	request: Request,
	next: middleware::Next,
) -> Response {
	// Build ETag for this request.
	let mut hasher = SeaHasher::new();
	uri.hash(&mut hasher);
	let uri_hash = hasher.finish();

	let etag = format!(
		"\"{uri_hash:016x}.{}.{ASSET_ETAG_VERSION}\"",
		snapshot.index.fingerprint()
	)
	.parse::<ETag>()
	.expect("malformed etag");

	// Validate the resource/request before returning 304. Never cache errors.
	let mut response = next.run(request).await;
	if !response.status().is_success() {
		return response;
	}
	if header_if_none_match.is_some_and(|TypedHeader(value)| !value.precondition_passes(&etag)) {
		response = StatusCode::NOT_MODIFIED.into_response();
	}

	// Add cache headers.
	let cache_control = CacheControl::new()
		.with_public()
		.with_immutable()
		.with_max_age(Duration::from_secs(config.maxage));

	let headers = response.headers_mut();
	headers.typed_insert(etag);
	headers.typed_insert(cache_control);

	response
}

#[cfg(test)]
mod tests {
	use super::*;
	use axum::{
		Router,
		body::{Body, to_bytes},
	};
	use bm_asset_index::{AssetRef, Reader, path_hash};
	use bytes::Bytes;
	use object_store::{ObjectStore, memory::InMemory, path::Path as ObjectPath};
	use std::{io::Cursor, sync::Arc};
	use tower::ServiceExt;

	async fn fixture() -> Router {
		fixture_with_format(false).await
	}

	async fn fixture_with_format(avif: bool) -> Router {
		let store = Arc::new(InMemory::new());
		let mut data = Cursor::new(Vec::new());
		image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
			2,
			2,
			image::Rgba([128, 80, 40, 255]),
		))
		.write_to(&mut data, image::ImageFormat::WebP)
		.unwrap();
		let data = if avif {
			let hex = include_str!("../../../bm_asset_fs/tests/fixtures/rgba.avif.hex").trim();
			(0..hex.len())
				.step_by(2)
				.map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
				.collect()
		} else {
			data.into_inner()
		};
		let source = AssetRef {
			sha256: [1; 32],
			format: if avif {
				bm_asset_index::Format::Avif
			} else {
				bm_asset_index::Format::Webp
			},
		};
		store
			.put(
				&ObjectPath::from(source.object_path()),
				Bytes::from(data).into(),
			)
			.await
			.unwrap();
		let mut keys = [
			"ui/icon/000000/000001.tex",
			"ui/icon/000000/000001_hr1.tex",
			"ui/map/s1d1/00/s1d100_m.tex",
			"ui/map/s1d1/00/s1d100m_m.tex",
		]
		.map(|path| path_hash(path).unwrap());
		keys.sort_unstable();
		let mut index = vec![0u8; 64];
		index[..4].copy_from_slice(b"IXAS");
		index[4..6].copy_from_slice(&1u16.to_le_bytes());
		index[6..8].copy_from_slice(&64u16.to_le_bytes());
		index[12..14].copy_from_slice(&44u16.to_le_bytes());
		index[14..16].copy_from_slice(&1u16.to_le_bytes());
		index[16..20].copy_from_slice(&(keys.len() as u32).to_le_bytes());
		index[20..24].copy_from_slice(&64u32.to_le_bytes());
		for key in keys {
			index.extend_from_slice(&key.to_le_bytes());
			index.extend_from_slice(&source.sha256);
			index.extend_from_slice(&[if avif { 2 } else { 1 }, 0, 0, 0]);
		}
		let length = index.len() as u32;
		index[24..28].copy_from_slice(&length.to_le_bytes());
		let checksum = crc32fast::hash(&index[64..]);
		index[28..32].copy_from_slice(&checksum.to_le_bytes());
		store
			.put(
				&ObjectPath::from("patches/1/assets.bin"),
				Bytes::from(index).into(),
			)
			.await
			.unwrap();
		let reader = Reader::with_store(store, "", None, Some("1".into()))
			.await
			.unwrap();
		let service = Arc::new(bm_asset::Service::from_reader(Some(Arc::new(reader))));
		let mut openapi = openapi::OpenApi::default();
		Router::new()
			.nest(
				"/api/asset",
				router(Config::default(), service.clone()).finish_api(&mut openapi),
			)
			.nest("/i", legacy_router(Config::default(), service))
	}

	#[tokio::test]
	async fn avif_source_routes_and_raw_legacy_response() {
		let app = fixture_with_format(true).await;
		for (format, mime, expected) in [
			("png", "image/png", image::ImageFormat::Png),
			("jpg", "image/jpeg", image::ImageFormat::Jpeg),
			("webp", "image/webp", image::ImageFormat::WebP),
		] {
			for url in [
				format!("/api/asset?path=ui/icon/000000/000001.tex&format={format}"),
				format!("/api/asset/map/s1d1/00?format={format}"),
			] {
				let response = app
					.clone()
					.oneshot(Request::builder().uri(&url).body(Body::empty()).unwrap())
					.await
					.unwrap();
				assert_eq!(response.status(), StatusCode::OK, "{url}");
				assert_eq!(response.headers()[header::CONTENT_TYPE], mime);
				let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
				assert_eq!(image::guess_format(&body).unwrap(), expected);
				assert_eq!(image::load_from_memory(&body).unwrap().width(), 2);
			}
		}
		let response = app
			.oneshot(
				Request::builder()
					.uri("/i/000000/000001.png")
					.body(Body::empty())
					.unwrap(),
			)
			.await
			.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		assert_eq!(response.headers()[header::CONTENT_TYPE], "image/avif");
		let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
		let expected = include_str!("../../../bm_asset_fs/tests/fixtures/rgba.avif.hex").trim();
		assert_eq!(
			body.iter().map(|b| format!("{b:02x}")).collect::<String>(),
			expected
		);
	}

	#[tokio::test]
	async fn avif_passthrough_preserves_bytes_headers_and_cache_behavior() {
		let app = fixture_with_format(true).await;
		let expected = include_str!("../../../bm_asset_fs/tests/fixtures/rgba.avif.hex").trim();
		for url in [
			"/api/asset?path=ui/icon/000000/000001.tex&format=avif",
			"/api/asset/ui/icon/000000/000001.tex?format=avif",
			"/api/asset?path=ui/map/s1d1/00/s1d100_m.tex&format=avif",
		] {
			let response = app
				.clone()
				.oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
				.await
				.unwrap();
			assert_eq!(response.status(), StatusCode::OK, "{url}");
			assert_eq!(response.headers()[header::CONTENT_TYPE], "image/avif");
			assert!(
				response.headers()[header::CONTENT_DISPOSITION]
					.to_str()
					.unwrap()
					.ends_with(".avif\"")
			);
			assert!(response.headers().contains_key(header::CACHE_CONTROL));
			let etag = response.headers()[header::ETAG].clone();
			let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
			assert_eq!(
				body.iter().map(|b| format!("{b:02x}")).collect::<String>(),
				expected
			);
			let response = app
				.clone()
				.oneshot(
					Request::builder()
						.uri(url)
						.header(header::IF_NONE_MATCH, etag)
						.body(Body::empty())
						.unwrap(),
				)
				.await
				.unwrap();
			assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
			assert!(
				to_bytes(response.into_body(), 1024)
					.await
					.unwrap()
					.is_empty()
			);
			let response = app
				.clone()
				.oneshot(
					Request::builder()
						.method("HEAD")
						.uri(url)
						.body(Body::empty())
						.unwrap(),
				)
				.await
				.unwrap();
			assert_eq!(response.status(), StatusCode::OK);
			assert_eq!(response.headers()[header::CONTENT_TYPE], "image/avif");
			assert!(
				to_bytes(response.into_body(), 1024)
					.await
					.unwrap()
					.is_empty()
			);
		}
	}

	#[tokio::test]
	async fn omitted_format_returns_stored_bytes_for_both_source_formats() {
		for avif in [false, true] {
			let app = fixture_with_format(avif).await;
			let original = app
				.clone()
				.oneshot(
					Request::builder()
						.uri("/i/000000/000001.png")
						.body(Body::empty())
						.unwrap(),
				)
				.await
				.unwrap();
			let stored = to_bytes(original.into_body(), 1024 * 1024).await.unwrap();
			for url in [
				"/api/asset?path=ui/icon/000000/000001.tex",
				"/api/asset/ui/icon/000000/000001.tex",
				"/api/asset?path=ui/map/s1d1/00/s1d100_m.tex",
			] {
				let response = app
					.clone()
					.oneshot(
						Request::builder()
							.uri(url)
							.header(header::ACCEPT, "image/png")
							.body(Body::empty())
							.unwrap(),
					)
					.await
					.unwrap();
				assert_eq!(response.status(), StatusCode::OK);
				assert_eq!(
					response.headers()[header::CONTENT_TYPE],
					if avif { "image/avif" } else { "image/webp" }
				);
				assert!(
					response.headers()[header::CONTENT_DISPOSITION]
						.to_str()
						.unwrap()
						.ends_with(if avif { ".avif\"" } else { ".webp\"" })
				);
				let etag = response.headers()[header::ETAG].clone();
				let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
				assert_eq!(body, stored, "{url}");
				let response = app
					.clone()
					.oneshot(
						Request::builder()
							.uri(url)
							.header(header::IF_NONE_MATCH, etag)
							.body(Body::empty())
							.unwrap(),
					)
					.await
					.unwrap();
				assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
				assert!(
					to_bytes(response.into_body(), 1024)
						.await
						.unwrap()
						.is_empty()
				);
				let response = app
					.clone()
					.oneshot(
						Request::builder()
							.method("HEAD")
							.uri(url)
							.body(Body::empty())
							.unwrap(),
					)
					.await
					.unwrap();
				assert_eq!(response.status(), StatusCode::OK);
				assert_eq!(
					response.headers()[header::CONTENT_TYPE],
					if avif { "image/avif" } else { "image/webp" }
				);
				assert!(
					to_bytes(response.into_body(), 1024)
						.await
						.unwrap()
						.is_empty()
				);
			}
			let response = app
				.clone()
				.oneshot(
					Request::builder()
						.uri("/api/asset/map/s1d1/00")
						.header(header::ACCEPT, "image/avif")
						.body(Body::empty())
						.unwrap(),
				)
				.await
				.unwrap();
			assert_eq!(response.status(), StatusCode::OK);
			assert_eq!(response.headers()[header::CONTENT_TYPE], "image/jpeg");
			let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
			assert_eq!(
				image::guess_format(&body).unwrap(),
				image::ImageFormat::Jpeg
			);
			for url in ["/i/000000/000001.png", "/i/000000/000001.png?format=png"] {
				let response = app
					.clone()
					.oneshot(
						Request::builder()
							.uri(url)
							.header(header::ACCEPT, "image/png")
							.body(Body::empty())
							.unwrap(),
					)
					.await
					.unwrap();
				assert_eq!(response.status(), StatusCode::OK);
				assert_eq!(
					response.headers()[header::CONTENT_TYPE],
					if avif { "image/avif" } else { "image/webp" }
				);
			}
		}
	}

	#[tokio::test]
	async fn avif_rejects_encoding_and_composition_requests() {
		for (avif, url, message) in [
			(
				false,
				"/api/asset?path=ui/icon/000000/000001.tex&format=avif",
				"AVIF encoding is not supported",
			),
			(false, "/api/asset/map/s1d1/00?format=avif", "composed maps"),
			(true, "/api/asset/map/s1d1/00?format=avif", "composed maps"),
		] {
			let response = fixture_with_format(avif)
				.await
				.oneshot(
					Request::builder()
						.uri(url)
						.header(header::IF_NONE_MATCH, "*")
						.body(Body::empty())
						.unwrap(),
				)
				.await
				.unwrap();
			assert_eq!(response.status(), StatusCode::BAD_REQUEST);
			assert!(!response.headers().contains_key(header::CACHE_CONTROL));
			let body = to_bytes(response.into_body(), 4096).await.unwrap();
			let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
			assert!(error["message"].as_str().unwrap().contains(message));
		}
	}

	#[test]
	fn schema_distinguishes_file_formats_from_map_formats() {
		let file = serde_json::to_value(schemars::schema_for!(AssetQuery)).unwrap();
		assert!(
			!file["required"]
				.as_array()
				.unwrap()
				.contains(&serde_json::json!("format"))
		);
		assert!(file.to_string().contains("avif"));
		let map = serde_json::to_value(schemars::schema_for!(MapQuery)).unwrap();
		assert_eq!(map["properties"]["format"]["default"], "jpg");
		assert_eq!(
			map["properties"]["format"]["enum"],
			serde_json::json!(["jpg", "png", "webp"])
		);
	}

	#[tokio::test]
	async fn upstream_formats_and_legacy_raw_icons() {
		let app = fixture().await;
		for (url, mime, format) in [
			(
				"/api/asset?path=ui/icon/000000/000001.tex&format=png",
				"image/png",
				image::ImageFormat::Png,
			),
			(
				"/api/asset/ui/icon/000000/000001.tex?format=webp",
				"image/webp",
				image::ImageFormat::WebP,
			),
			(
				"/api/asset/map/s1d1/00",
				"image/jpeg",
				image::ImageFormat::Jpeg,
			),
			(
				"/api/asset/map/s1d1/00?format=png&version=1",
				"image/png",
				image::ImageFormat::Png,
			),
			(
				"/i/999999/000001.png",
				"image/webp",
				image::ImageFormat::WebP,
			),
			(
				"/i/000000/000001_hr1.png",
				"image/webp",
				image::ImageFormat::WebP,
			),
		] {
			let response = app
				.clone()
				.oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
				.await
				.unwrap();
			assert_eq!(response.status(), StatusCode::OK, "{url}");
			assert_eq!(response.headers()[header::CONTENT_TYPE], mime);
			assert!(response.headers().contains_key(header::ETAG));
			let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
			assert_eq!(image::guess_format(&body).unwrap(), format);
			if url.contains("map") && format == image::ImageFormat::Png {
				assert_eq!(
					image::load_from_memory(&body)
						.unwrap()
						.into_rgba8()
						.get_pixel(0, 0)
						.0,
					[64, 25, 6, 255]
				);
			}
		}
	}

	#[tokio::test]
	async fn cache_validation_and_errors() {
		let app = fixture().await;
		let url = "/api/asset?path=ui/icon/000000/000001.tex&format=png";
		let response = app
			.clone()
			.oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
			.await
			.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		let etag = response.headers()[header::ETAG].clone();
		let response = app
			.clone()
			.oneshot(
				Request::builder()
					.uri(url)
					.header(header::IF_NONE_MATCH, etag)
					.body(Body::empty())
					.unwrap(),
			)
			.await
			.unwrap();
		assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
		assert!(
			to_bytes(response.into_body(), 1024)
				.await
				.unwrap()
				.is_empty()
		);
		let response = app
			.clone()
			.oneshot(
				Request::builder()
					.method("HEAD")
					.uri(url)
					.body(Body::empty())
					.unwrap(),
			)
			.await
			.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		assert!(
			to_bytes(response.into_body(), 1024)
				.await
				.unwrap()
				.is_empty()
		);
		for (url, status) in [
			("/api/asset?format=png", StatusCode::BAD_REQUEST),
			(
				"/api/asset?path=ui/icon/000000/000001.tex&format=",
				StatusCode::BAD_REQUEST,
			),
			(
				"/api/asset?path=ui/icon/000000/000001.tex&format=gif",
				StatusCode::BAD_REQUEST,
			),
			(
				"/api/asset?path=ui/icon/000000/000001.tex&format=avif",
				StatusCode::BAD_REQUEST,
			),
			(
				"/api/asset?path=ui/icon/000000/000001.tex&format=png&version=missing",
				StatusCode::BAD_REQUEST,
			),
			(
				"/api/asset?path=ui/../secret.tex&format=png",
				StatusCode::BAD_REQUEST,
			),
			(
				"/api/asset?path=ui/icon/000000/999999.tex&format=png",
				StatusCode::NOT_FOUND,
			),
			("/i/000000/000002.png", StatusCode::NOT_FOUND),
			("/i/000000/000001.webp", StatusCode::NOT_FOUND),
		] {
			let response = app
				.clone()
				.oneshot(
					Request::builder()
						.uri(url)
						.header(header::IF_NONE_MATCH, "*")
						.body(Body::empty())
						.unwrap(),
				)
				.await
				.unwrap();
			assert_eq!(response.status(), status, "{url}");
			assert!(!response.headers().contains_key(header::CACHE_CONTROL));
		}
	}
}
