use super::base::BaseTemplate;
use crate::{http::HttpState, service::Service};
use axum::{
	Router,
	extract::{Path, State},
	http::StatusCode,
	response::{IntoResponse, Response},
	routing::get,
};
use maud::{Render, html};

pub fn router(state: HttpState) -> Router {
	Router::new().route("/{version_key}", get(version).with_state(state))
}

async fn version(Path(key): Path<String>, State(Service { data, .. }): State<Service>) -> Response {
	let versions = data.versions();
	let Some(version) = versions.iter().find(|version| version.key == key) else {
		return StatusCode::NOT_FOUND.into_response();
	};
	BaseTemplate {
        title: format!("version {key}"),
        content: html! {
            dl {
                dt { "key" } dd { (version.key) }
                dt { "version" } dd { (version.version) }
                dt { "published" } dd { (version.published_at) }
                dt { "active" } dd { (versions.first().is_some_and(|v| v.key == key)) }
            }
            p { "Versions are downloaded from ixion releases. Old releases are removed automatically; data requests always use the latest release." }
        },
    }.render().into_response()
}
