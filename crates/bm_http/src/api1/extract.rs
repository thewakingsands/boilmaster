use aide::OperationIo;
use axum::{
	extract::{FromRef, FromRequestParts},
	http::request::Parts,
};
use bm_version::VersionKey;

use crate::service::Service;

use super::error::Error;

#[derive(OperationIo)]
#[aide(input_with = "Query<()>")]
pub struct VersionQuery(pub VersionKey);

impl<S> FromRequestParts<S> for VersionQuery
where
	S: Send + Sync,
	Service: FromRef<S>,
{
	type Rejection = Error;

	async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
		let Service { data, .. } = Service::from_ref(state);
		let key = *parts.extensions.get_or_insert_with(|| data.version_key());
		Ok(Self(key))
	}
}

#[derive(FromRequestParts, OperationIo)]
#[from_request(via(axum::extract::Path), rejection(Error))]
#[aide(input_with = "axum::extract::Path<T>", json_schema)]
pub struct Path<T>(pub T);

#[derive(FromRequestParts, OperationIo)]
#[from_request(via(axum::extract::Query), rejection(Error))]
#[aide(input_with = "axum::extract::Query<T>", json_schema)]
pub struct Query<T>(pub T);
