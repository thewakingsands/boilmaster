use std::sync::Arc;

use axum::{
	Json, Router,
	extract::State,
	http::{HeaderValue, StatusCode, header::CACHE_CONTROL},
	middleware,
	response::Response,
	routing::{get, post},
};

use crate::admin::docs::{Docs, Status};

pub(crate) fn router(docs: Arc<Docs>, token: Option<String>) -> Router {
	Router::new()
		.route(
			"/docs/update",
			get(status).merge(super::update_auth::protect(post(update), token)),
		)
		.with_state(docs)
		.layer(middleware::map_response(|mut response: Response| async {
			response
				.headers_mut()
				.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
			response
		}))
}

async fn status(State(docs): State<Arc<Docs>>) -> Json<Status> {
	Json(docs.status())
}

async fn update(State(docs): State<Arc<Docs>>) -> (StatusCode, Json<Status>) {
	(StatusCode::ACCEPTED, Json(docs.trigger()))
}
