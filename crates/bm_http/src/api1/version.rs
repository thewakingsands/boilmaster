use aide::{
	axum::{ApiRouter, routing::get_with},
	transform::TransformOperation,
};
use axum::{
	Json,
	extract::State,
	http::StatusCode,
	response::{IntoResponse, Response},
	routing::post,
};
use bm_data::{LocalVersion, UpdateStatus};
use schemars::JsonSchema;
use serde::Serialize;

use super::api::ApiState;
use crate::service::Service;

pub fn router(state: ApiState) -> ApiRouter {
	ApiRouter::new()
		.api_route(
			"/",
			get_with(versions, versions_docs).with_state(state.clone()),
		)
		.route(
			"/",
			super::update_auth::protect(post(update), std::env::var("BM_UPDATE_TOKEN").ok())
				.with_state(state),
		)
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

async fn update(State(Service { data, .. }): State<Service>) -> Response {
	let status = data.trigger_update();
	(StatusCode::ACCEPTED, Json(payload(&data, status))).into_response()
}
