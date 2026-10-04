use axum::{
	extract::{Request, State},
	http::{Method, StatusCode, header},
	middleware::Next,
	response::{IntoResponse, Response},
};
use axum_extra::{
	TypedHeader,
	headers::{Authorization, authorization::Basic},
};
use serde::Deserialize;
use subtle::ConstantTimeEq;

#[derive(Default, Deserialize, Clone)]
#[serde(default)]
pub struct BasicAuth {
	pub username: String,
	pub password: String,
}
impl std::fmt::Debug for BasicAuth {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("BasicAuth").finish_non_exhaustive()
	}
}
#[derive(Clone)]
pub struct Guard {
	pub auth: BasicAuth,
	pub csrf: String,
}
fn equal(expected: &str, supplied: &str) -> bool {
	!expected.is_empty() && bool::from(expected.as_bytes().ct_eq(supplied.as_bytes()))
}
pub async fn protect(
	State(guard): State<Guard>,
	authorization: Option<TypedHeader<Authorization<Basic>>>,
	request: Request,
	next: Next,
) -> Response {
	let authenticated = authorization.is_some_and(|TypedHeader(auth)| {
		equal(&guard.auth.username, auth.username()) & equal(&guard.auth.password, auth.password())
	});
	let mut response = if !authenticated {
		(
			StatusCode::UNAUTHORIZED,
			[(
				header::WWW_AUTHENTICATE,
				"Basic realm=\"boilmaster admin\", charset=\"UTF-8\"",
			)],
		)
			.into_response()
	} else if request.method() != Method::GET
		&& request.method() != Method::HEAD
		&& !request
			.headers()
			.get("x-csrf-token")
			.and_then(|v| v.to_str().ok())
			.is_some_and(|token| equal(&guard.csrf, token))
	{
		(StatusCode::FORBIDDEN, "Missing or invalid CSRF token").into_response()
	} else {
		next.run(request).await
	};
	let headers = response.headers_mut();
	headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
	headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
	headers.insert(header::CONTENT_SECURITY_POLICY, "default-src 'self'; script-src 'self'; style-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'".parse().unwrap());
	response
}
#[cfg(test)]
mod tests {
	use super::*;
	use axum::{Router, body::Body, middleware, routing::get};
	use tower::ServiceExt;
	#[tokio::test]
	async fn requires_auth_and_csrf_for_writes() {
		let app = Router::new()
			.route("/", get(|| async { "ok" }).post(|| async { "updated" }))
			.layer(middleware::from_fn_with_state(
				Guard {
					auth: BasicAuth {
						username: "admin".into(),
						password: "test".into(),
					},
					csrf: "token".into(),
				},
				protect,
			));
		for (method, auth, csrf, expected) in [
			("GET", None, None, 401),
			("GET", Some("Basic YWRtaW46YmFk"), None, 401),
			("GET", Some("Basic YWRtaW46dGVzdA=="), None, 200),
			("POST", Some("Basic YWRtaW46dGVzdA=="), None, 403),
			("POST", Some("Basic YWRtaW46dGVzdA=="), Some("wrong"), 403),
			("POST", Some("Basic YWRtaW46dGVzdA=="), Some("token"), 200),
		] {
			let mut request = Request::builder().uri("/").method(method);
			if let Some(auth) = auth {
				request = request.header(header::AUTHORIZATION, auth);
			}
			if let Some(csrf) = csrf {
				request = request.header("x-csrf-token", csrf);
			}
			let response = app
				.clone()
				.oneshot(request.body(Body::empty()).unwrap())
				.await
				.unwrap();
			assert_eq!(response.status().as_u16(), expected);
			assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
		}
		assert!(!equal("", ""));
	}
}
