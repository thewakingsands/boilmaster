//! Shared authentication for CI-triggered updates.
use axum::{
	Json,
	extract::{Query, Request, State},
	http::{HeaderMap, StatusCode},
	middleware::{self, Next},
	response::{IntoResponse, Response},
	routing::MethodRouter,
};
use serde::Deserialize;

pub(super) fn protect<S>(route: MethodRouter<S>, token: Option<String>) -> MethodRouter<S>
where
	S: Clone + Send + Sync + 'static,
{
	route.route_layer(middleware::from_fn_with_state(token, authenticate))
}

#[derive(Deserialize)]
struct UpdateQuery {
	token: Option<String>,
}

async fn authenticate(
	State(expected): State<Option<String>>,
	Query(query): Query<UpdateQuery>,
	headers: HeaderMap,
	request: Request,
	next: Next,
) -> Response {
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
	next.run(request).await
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

#[cfg(test)]
mod tests {
	use super::*;
	use axum::{Router, body::Body, routing::post};
	use tower::ServiceExt;

	#[tokio::test]
	async fn update_requires_nonempty_matching_token() {
		for (expected, query, bearer, accepted) in [
			(None, "", None, false),
			(Some(""), "?token=", None, false),
			(Some("secret"), "", None, false),
			(Some("secret"), "?token=secreT", None, false),
			(Some("secret"), "?token=secret-extra", None, false),
			(Some("secret"), "?token=secret", None, true),
			(Some("secret"), "", Some("Bearer secret"), true),
			(Some("secret"), "?token=secret", Some("Bearer wrong"), false),
		] {
			let app = Router::new().route(
				"/update",
				protect(
					post(|| async { StatusCode::ACCEPTED }),
					expected.map(str::to_owned),
				),
			);
			let mut request = Request::builder()
				.method("POST")
				.uri(format!("/update{query}"));
			if let Some(bearer) = bearer {
				request = request.header("Authorization", bearer);
			}
			let response = app
				.oneshot(request.body(Body::empty()).unwrap())
				.await
				.unwrap();
			assert_eq!(
				response.status(),
				if accepted {
					StatusCode::ACCEPTED
				} else {
					StatusCode::UNAUTHORIZED
				}
			);
		}
	}
}
