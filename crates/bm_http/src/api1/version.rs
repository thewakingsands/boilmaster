use aide::{
	axum::{ApiRouter, routing::get_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::{Query, State},
	http::{HeaderMap, StatusCode},
	response::{IntoResponse, Response},
	routing::post,
};
use bm_data::{LocalVersion, UpdateStatus};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::api::ApiState;
use crate::service::Service;

pub fn router(state: ApiState) -> ApiRouter {
	ApiRouter::new()
		.api_route(
			"/",
			get_with(versions, versions_docs).with_state(state.clone()),
		)
		.route("/", post(update).with_state(state))
}

#[derive(Serialize, JsonSchema)]
struct VersionMetadata {
	#[serde(flatten)]
	metadata: LocalVersion,
	names: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
struct VersionsResponse {
	key: Option<String>,
	version: Option<String>,
	versions: Vec<VersionMetadata>,
	update: UpdateStatus,
}

fn payload(data: &bm_data::Data, update: UpdateStatus) -> VersionsResponse {
	let versions = data.versions();
	VersionsResponse {
		key: versions.first().map(|v| v.key.clone()),
		version: versions.first().map(|v| v.version.clone()),
		versions: versions
			.into_iter()
			.enumerate()
			.map(|(i, metadata)| VersionMetadata {
				metadata,
				names: if i == 0 {
					vec!["latest".into()]
				} else {
					vec![]
				},
			})
			.collect(),
		update,
	}
}

fn versions_docs(operation: TransformOperation) -> TransformOperation {
	operation.summary("list local versions and update status")
        .description("Lists up to ten downloaded releases, newest first. Data requests always use the latest local release.")
        .response::<200, Json<VersionsResponse>>()
}

async fn versions(State(Service { data, .. }): State<Service>) -> Json<VersionsResponse> {
	Json(payload(&data, data.update_status()))
}

#[derive(Deserialize)]
struct UpdateQuery {
	token: Option<String>,
}

fn authorized(expected: Option<&str>, supplied: Option<&str>) -> bool {
	match (expected.filter(|s| !s.is_empty()), supplied) {
		(Some(expected), Some(supplied)) => {
			// Compare all bytes without early exit on the first differing byte.
			let mut difference = expected.len() ^ supplied.len();
			for (i, byte) in expected.bytes().enumerate() {
				difference |= usize::from(byte ^ supplied.as_bytes().get(i).copied().unwrap_or(0));
			}
			difference == 0
		}
		_ => false,
	}
}

async fn update(
	State(Service { data, .. }): State<Service>,
	Query(query): Query<UpdateQuery>,
	headers: HeaderMap,
) -> Response {
	let expected = std::env::var("BM_UPDATE_TOKEN").ok();
	let bearer = headers
		.get("authorization")
		.and_then(|v| v.to_str().ok())
		.and_then(|v| v.strip_prefix("Bearer "));
	if !authorized(expected.as_deref(), bearer.or(query.token.as_deref())) {
		return (
			StatusCode::UNAUTHORIZED,
			Json(serde_json::json!({"error": "unauthorized"})),
		)
			.into_response();
	}
	let status = data.trigger_update();
	(StatusCode::ACCEPTED, Json(payload(&data, status))).into_response()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn update_requires_nonempty_matching_token() {
		assert!(!authorized(None, None));
		assert!(!authorized(Some(""), Some("")));
		assert!(!authorized(Some("secret"), None));
		assert!(!authorized(Some("secret"), Some("secreT")));
		assert!(!authorized(Some("secret"), Some("secret-extra")));
		assert!(authorized(Some("secret"), Some("secret")));
	}
}
