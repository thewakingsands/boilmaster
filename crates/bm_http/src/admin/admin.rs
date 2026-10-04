use super::{
	auth::{BasicAuth, Guard, protect},
	docs::{self, Docs},
};
use crate::service;
use axum::{
	Json, Router,
	extract::State,
	http::{StatusCode, header},
	middleware,
	response::{IntoResponse, Response},
	routing::{get, post},
};
use maud::{DOCTYPE, Markup, html};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
	pub enabled: bool,
	pub auth: BasicAuth,
	pub docs: docs::Config,
}
#[derive(Clone)]
struct AdminState {
	data: service::Data,
	asset: service::Asset,
	docs: Arc<Docs>,
	csrf: String,
}

pub fn router(
	config: Config,
	data: service::Data,
	asset: service::Asset,
	docs: Arc<Docs>,
) -> anyhow::Result<Router> {
	if !config.enabled {
		return Ok(Router::new().fallback(|| async { StatusCode::NOT_FOUND }));
	}
	anyhow::ensure!(
		!config.auth.username.is_empty() && !config.auth.password.is_empty(),
		"admin requires a nonempty username and password"
	);
	let csrf = uuid::Uuid::new_v4().to_string();
	let guard = Guard {
		auth: config.auth,
		csrf: csrf.clone(),
	};
	let state = AdminState {
		data,
		asset,
		docs,
		csrf,
	};
	Ok(Router::new()
		.route("/", get(dashboard))
		.route("/status", get(status))
		.route("/data/update", post(update_data))
		.route("/assets/reload", post(reload_assets))
		.route("/docs/sync", post(sync_docs))
		.route(
			"/admin.js",
			get(|| async {
				(
					[(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
					include_str!("admin.js"),
				)
			}),
		)
		.route(
			"/admin.css",
			get(|| async {
				(
					[(header::CONTENT_TYPE, "text/css; charset=utf-8")],
					include_str!("admin.css"),
				)
			}),
		)
		.layer(middleware::from_fn_with_state(guard, protect))
		.with_state(state))
}
async fn status(State(state): State<AdminState>) -> Json<serde_json::Value> {
	Json(
		serde_json::json!({ "data": { "versions": state.data.versions(), "update": state.data.update_status() }, "assets": state.asset.status(), "docs": state.docs.status() }),
	)
}
async fn update_data(State(state): State<AdminState>) -> impl IntoResponse {
	(StatusCode::ACCEPTED, Json(state.data.trigger_update()))
}
async fn reload_assets(State(state): State<AdminState>) -> Response {
	if state.asset.status().pinned {
		return (
			StatusCode::CONFLICT,
			"Assets version is pinned by BM_ASSET_VERSION; change the configuration to follow current.json.",
		)
			.into_response();
	}
	match state.asset.reload().await {
		Ok(_) => Json(state.asset.status()).into_response(),
		Err(error) => (StatusCode::CONFLICT, error.to_string()).into_response(),
	}
}
async fn sync_docs(State(state): State<AdminState>) -> impl IntoResponse {
	(StatusCode::ACCEPTED, Json(state.docs.trigger()))
}
async fn dashboard(State(state): State<AdminState>) -> Markup {
	let data = state.data.versions();
	let update = state.data.update_status();
	let assets = state.asset.status();
	let docs = state.docs.status();
	html! {
		(DOCTYPE)
		html lang="zh-CN" {
			head { meta charset="utf-8"; meta name="viewport" content="width=device-width, initial-scale=1";
				meta name="csrf-token" content=(state.csrf); title { "boilmaster 管理" }
				link rel="stylesheet" href="/admin/admin.css"; script src="/admin/admin.js" defer {} }
			body { main {
				header { h1 { "boilmaster 管理" } p { "数据、图片索引与文档站。更新失败时继续提供上一份有效内容。" } }
				p id="notice" role="status" aria-live="polite" {}
				section {
					h2 { "数据文件" }
					p { "使用最新本地版本，最多保留 10 个版本；更新会检查 ixion 的最新稳定 Release。" }
					p { "更新状态：" span id="data-status" { (update.state) } }
					button data-action="/admin/data/update" disabled[update.state == "running"] { "检查并更新数据" }
					div class="table-scroll" { table {
						thead { tr { th { "版本" } th { "Release" } th { "发布时间" } th { "状态" } } }
						tbody { @for (i, version) in data.iter().enumerate() { tr { td { (version.version) } td { (version.key) } td { (version.published_at) } td { @if i == 0 { "当前" } @else { "已保留" } } } } }
					} }
				}
				section {
					h2 { "Assets 索引" }
					p { @if !assets.enabled { "图片服务未启用。" } @else if assets.pinned { "已配置固定版本，自动及手动重载均不会切换版本。" } @else { "跟随 current.json，每小时自动重载；也可立即检查同版本索引更新。" } }
					p { "重载状态：" span id="assets-status" { @if assets.reload.running { "进行中" } @else { "就绪" } } }
					button data-action="/admin/assets/reload" disabled[!assets.enabled || assets.pinned || assets.reload.running] { "立即重载索引" }
					p { "下表仅列出本进程已加载的索引，不枚举 MinIO 中的全部版本。" }
					div class="table-scroll" { table {
						thead { tr { th { "游戏版本" } th { "条目数" } th { "索引 SHA-256" } th { "状态" } } }
						tbody { @for index in &assets.indexes { tr { td { (index.version) } td { (index.entries) } td { code { (index.fingerprint) } } td { @if index.current { "当前" } @else { "已缓存" } } } } }
					} }
				}
				section {
					h2 { "文档站" }
					@if let Some(current) = &docs.current { p { "当前 Release：" strong { (current.tag) } " · " (current.published_at) } p { "校验值：" code { (current.sha256) } } }
					@else { p { "当前使用挂载目录中的原有静态文件，尚未同步 Release。" } }
					p { "同步状态：" span id="docs-status" { @if docs.running { "进行中" } @else { "就绪" } } }
					p { "匿名下载最新稳定 Release 的 docs.zip，校验 SHA-256 和站点入口后切换。需要静态目录可写；旧文档会保留，页面可能仍受浏览器/CDN 缓存影响。" }
					button data-action="/admin/docs/sync" disabled[docs.running] { "同步最新文档" }
				}
				footer { a href="/zh-cn/" { "文档站" } " · " a href="/api/docs" { "API 参考" } }
			} }
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use axum::{
		body::{Body, to_bytes},
		extract::Request,
	};
	use tower::ServiceExt;

	#[tokio::test]
	async fn admin_routes_are_protected_and_render_controls() {
		let directory = tempfile::tempdir().unwrap();
		let data = Arc::new(bm_data::Data::new(directory.path().to_path_buf().into()));
		let asset = Arc::new(bm_asset::Service::from_reader(None));
		let docs = Docs::new(directory.path().into(), docs::Config::default()).unwrap();
		let config = Config {
			enabled: true,
			auth: BasicAuth {
				username: "admin".into(),
				password: "test".into(),
			},
			..Default::default()
		};
		let app = Router::new().nest("/admin", router(config, data, asset, docs).unwrap());
		for path in [
			"/admin",
			"/admin/status",
			"/admin/admin.js",
			"/admin/admin.css",
		] {
			let response = app
				.clone()
				.oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
				.await
				.unwrap();
			assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
		}
		let response = app
			.clone()
			.oneshot(
				Request::builder()
					.uri("/admin")
					.header(header::AUTHORIZATION, "Basic YWRtaW46dGVzdA==")
					.body(Body::empty())
					.unwrap(),
			)
			.await
			.unwrap();
		assert_eq!(response.status(), StatusCode::OK);
		let body = String::from_utf8(
			to_bytes(response.into_body(), 100_000)
				.await
				.unwrap()
				.to_vec(),
		)
		.unwrap();
		for expected in [
			"数据文件",
			"Assets 索引",
			"文档站",
			"/admin/data/update",
			"/admin/assets/reload",
			"/admin/docs/sync",
			"csrf-token",
		] {
			assert!(body.contains(expected));
		}
		for path in [
			"/admin/data/update",
			"/admin/assets/reload",
			"/admin/docs/sync",
		] {
			let response = app
				.clone()
				.oneshot(
					Request::builder()
						.method("POST")
						.uri(path)
						.header(header::AUTHORIZATION, "Basic YWRtaW46dGVzdA==")
						.body(Body::empty())
						.unwrap(),
				)
				.await
				.unwrap();
			assert_eq!(response.status(), StatusCode::FORBIDDEN);
		}
		let token = body
			.split("name=\"csrf-token\" content=\"")
			.nth(1)
			.unwrap()
			.split('"')
			.next()
			.unwrap();
		let response = app
			.clone()
			.oneshot(
				Request::builder()
					.method("POST")
					.uri("/admin/assets/reload")
					.header(header::AUTHORIZATION, "Basic YWRtaW46dGVzdA==")
					.header("x-csrf-token", token)
					.body(Body::empty())
					.unwrap(),
			)
			.await
			.unwrap();
		assert_eq!(response.status(), StatusCode::CONFLICT); // Disabled service, not an auth failure.
		let response = app
			.oneshot(
				Request::builder()
					.uri("/admin/status")
					.header(header::AUTHORIZATION, "Basic YWRtaW46dGVzdA==")
					.body(Body::empty())
					.unwrap(),
			)
			.await
			.unwrap();
		let json: serde_json::Value =
			serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await.unwrap())
				.unwrap();
		assert_eq!(json["assets"]["enabled"], false);
		assert_eq!(json["data"]["versions"], serde_json::json!([]));
	}

	#[tokio::test]
	async fn disabled_dashboard_is_not_served_and_empty_credentials_fail() {
		let directory = tempfile::tempdir().unwrap();
		let data = Arc::new(bm_data::Data::new(directory.path().to_path_buf().into()));
		let asset = Arc::new(bm_asset::Service::from_reader(None));
		let docs = Docs::new(directory.path().into(), docs::Config::default()).unwrap();
		assert!(
			router(
				Config {
					enabled: true,
					..Default::default()
				},
				data.clone(),
				asset.clone(),
				docs.clone()
			)
			.is_err()
		);
		let app = Router::new().nest(
			"/admin",
			router(Config::default(), data, asset, docs).unwrap(),
		);
		let response = app
			.oneshot(
				Request::builder()
					.uri("/admin")
					.body(Body::empty())
					.unwrap(),
			)
			.await
			.unwrap();
		assert_eq!(response.status(), StatusCode::NOT_FOUND);
	}
}
