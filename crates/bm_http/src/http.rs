use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::Result;
use axum::{
	Router,
	extract::{FromRef, MatchedPath},
	http::Request,
};
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower_http::services::ServeDir;
use tower_http::trace::{DefaultOnFailure, DefaultOnRequest, DefaultOnResponse, TraceLayer};
use tracing::Level;

use super::{api1, health, service};

#[derive(Debug, Deserialize)]
pub struct Config {
	api1: api1::Config,

	address: Option<IpAddr>,
	port: u16,
	directory: Option<String>,
}

#[derive(Clone, FromRef)]
pub struct HttpState {
	pub services: service::Service,
}

#[allow(clippy::too_many_arguments)]
pub async fn serve(
	cancel: CancellationToken,
	config: Config,
	asset: service::Asset,
	data: service::Data,
	read: service::Read,
	schema: service::Schema,
	search: service::Search,
) -> Result<()> {
	let bind_address = SocketAddr::new(
		config.address.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
		config.port,
	);
	let directory = config.directory.unwrap_or_else(|| "static".into());

	tracing::info!("http binding to {bind_address:?}");

	let state = HttpState {
		services: service::Service {
			asset,
			data,
			read,
			schema,
			search,
		},
	};

	let router = Router::new()
		.nest("/api", api1::router(config.api1, state.clone()))
		.nest("/health", health::router(state))
		.fallback_service(ServeDir::new(directory))
		.layer(
			TraceLayer::new_for_http()
				// Add the matched route path to the spans.
				.make_span_with(|request: &Request<_>| {
					let route = request
						.extensions()
						.get::<MatchedPath>()
						.map(MatchedPath::as_str);

					tracing::info_span!(
						"request",
						method = %request.method(),
						route,
						path = %request.uri().path(),
					)
				})
				// Downgrade access logs to TRACE.
				// TODO: Configurable? Should I just use envlogger syntax and call it a day?
				.on_request(DefaultOnRequest::new().level(Level::TRACE))
				.on_response(DefaultOnResponse::new().level(Level::TRACE))
				.on_failure(DefaultOnFailure::new().level(Level::TRACE)),
		);

	let listener = TcpListener::bind(bind_address).await.unwrap();
	axum::serve(listener, router)
		.with_graceful_shutdown(cancel.cancelled_owned())
		.await
		.unwrap();

	Ok(())
}
