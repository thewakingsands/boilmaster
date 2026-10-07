use std::sync::Arc;

use aide::{axum::ApiRouter, openapi, transform::TransformOpenApi};
use axum::{
	Json, Router, debug_handler,
	extract::{FromRef, State},
	http::Uri,
	response::IntoResponse,
	routing::get,
};
use git_version::git_version;
use maud::{DOCTYPE, html};
use regex::Regex;
use serde::Deserialize;
use tower_http::cors::CorsLayer;

use crate::{http::HttpState, service::Service};

use super::{asset, read::RowReaderState, search, sheet, version};

const OPENAPI_JSON_ROUTE: &str = "/openapi.json";

#[derive(Debug, Deserialize)]
pub struct Config {
	#[serde(default)]
	pub asset: asset::Config,
	search: search::Config,
	sheet: sheet::Config,
}

#[derive(Clone, FromRef)]
pub struct ApiState {
	pub services: Service,
	pub reader_state: RowReaderState,
}

pub fn router(config: Config, state: HttpState, docs: Arc<crate::admin::docs::Docs>) -> Router {
	let mut openapi = openapi::OpenApi::default();

	let state = ApiState {
		services: state.services,
		reader_state: RowReaderState::default(),
	};

	ApiRouter::new()
		.nest(
			"/asset",
			asset::router(config.asset, state.services.asset.clone())
				.with_path_items(|item| item.tag("assets")),
		)
		.nest(
			"/version",
			version::router(state.clone()).with_path_items(|item| item.tag("versions")),
		)
		.nest(
			"/versions",
			version::router(state.clone()).with_path_items(|item| item.tag("versions")),
		)
		.nest(
			"/search",
			search::router(config.search, state.clone()).with_path_items(|item| item.tag("search")),
		)
		.nest(
			"/sheet",
			sheet::router(config.sheet, state.clone()).with_path_items(|item| item.tag("sheets")),
		)
		.finish_api_with(&mut openapi, api_docs)
		.route(
			OPENAPI_JSON_ROUTE,
			get(openapi_json).with_state(OpenApiState {
				openapi: Arc::new(openapi),
			}),
		)
		.merge(super::docs::router(
			docs,
			std::env::var("BM_UPDATE_TOKEN").ok(),
		))
		.layer(CorsLayer::permissive())
		.route("/docs", get(scalar))
}

fn api_docs(api: TransformOpenApi) -> TransformOpenApi {
	let mut api = api
		.title("boilmaster")
		.version(git_version!(prefix = "1-", fallback = "unknown"))
		.tag(openapi::Tag {
			name: "assets".into(),
			description: Some("Game textures and composed maps from the IXAS index.".into()),
			..Default::default()
		})
		.tag(openapi::Tag {
			name: "search".into(),
			description: Some(
				"Endpoints for seaching and filtering the game's static relational data store."
					.into(),
			),
			..Default::default()
		})
		.tag(openapi::Tag {
			name: "sheets".into(),
			description: Some(
				"Endpoints for reading data from the game's static relational data store.".into(),
			),
			..Default::default()
		});

	let openapi = api.inner_mut();

	let wildcard_regex = Regex::new(r#"\*(?<name>\w+)$"#).unwrap();

	if let Some(paths) = openapi.paths.take() {
		openapi.paths = Some(openapi::Paths {
			paths: paths
				.paths
				.into_iter()
				.map(|(path, item)| {
					// Ensure we've not ended up with any trailing slashes.
					let path = path.trim_end_matches('/');
					// Replace any missed `*wildcard`s with openapi compatible syntax
					let path = wildcard_regex.replace(path, "{$name}");
					(path.into(), item)
				})
				.collect(),
			..paths
		})
	}

	api
}

#[derive(Clone, FromRef)]
struct OpenApiState {
	openapi: Arc<openapi::OpenApi>,
}

#[debug_handler]
async fn openapi_json(State(openapi): State<Arc<openapi::OpenApi>>) -> impl IntoResponse {
	Json(openapi.as_ref()).into_response()
}

#[debug_handler]
async fn scalar(uri: Uri) -> impl IntoResponse {
	html! {
		(DOCTYPE)
		html {
			head {
				title { "Boilmaster Documentation" }
				meta charset="utf-8";
				meta name="viewport" content="width=device-width, initial-scale=1";
			}
			body {
				script id="api-reference" data-url={ "." (OPENAPI_JSON_ROUTE) } {}
				// This script sets the configuration with a new server url based on
				// what the browser can see - this ensures that regardless of what
				// reverse proxies may be doing, the urls will be relative to the API's
				// mount point. I take no responsibility for any further fuckery.
				script {
					"var route = '" (uri) "';"
					r#"
					var serverUrl = location.origin + location.pathname.replace(new RegExp(route + '$'), '');
					var configuration = { servers: [{ url: serverUrl }] };
					document.getElementById('api-reference').dataset.configuration = JSON.stringify(configuration);
					"#
				}
				script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference" {}
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use axum::{
		body::{Body, to_bytes},
		http::{Request, StatusCode},
	};
	use tower::ServiceExt;

	#[tokio::test]
	async fn documentation_update_keeps_scalar_and_public_status_available() {
		let directory = tempfile::tempdir().unwrap();
		let docs =
			crate::admin::docs::Docs::new(directory.path().into(), Default::default()).unwrap();
		let app = Router::new().nest(
			"/api",
			super::super::docs::router(docs.clone(), None).route("/docs", get(scalar)),
		);
		for (path, expected_content_type) in [
			("/api/docs", "text/html"),
			("/api/docs/update", "application/json"),
		] {
			let response = app
				.clone()
				.oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
				.await
				.unwrap();
			assert_eq!(response.status(), StatusCode::OK);
			assert!(
				response.headers()["content-type"]
					.to_str()
					.unwrap()
					.starts_with(expected_content_type)
			);
			let body = to_bytes(response.into_body(), 8192).await.unwrap();
			if path == "/api/docs" {
				assert!(
					std::str::from_utf8(&body)
						.unwrap()
						.contains("api-reference")
				);
			} else {
				let status: serde_json::Value = serde_json::from_slice(&body).unwrap();
				assert_eq!(status["running"], false);
				assert!(status["job_id"].is_null());
			}
		}
		let response = app
			.oneshot(
				Request::builder()
					.method("POST")
					.uri("/api/docs/update?token=anything")
					.body(Body::empty())
					.unwrap(),
			)
			.await
			.unwrap();
		assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
		assert!(!docs.status().running);
	}
}
