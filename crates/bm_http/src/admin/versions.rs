use super::base::BaseTemplate;
use crate::{http::HttpState, service::Service};
use axum::{Router, extract::State, routing::get};
use maud::{Markup, Render, html};

pub fn router(state: HttpState) -> Router {
	Router::new().route("/", get(versions).with_state(state))
}

async fn versions(State(Service { data, .. }): State<Service>) -> Markup {
	let versions = data.versions();
	BaseTemplate {
		title: "versions".into(),
		content: html! {
			p { "Only the latest local release is used for data requests. Up to 10 releases are retained." }
			p { "Update state: " (data.update_status().state) }
			table.striped {
				thead { tr { th { "key" } th { "version" } th { "published" } th { "active" } } }
				tbody {
					@for (i, version) in versions.iter().enumerate() {
						tr {
							td { a href={ "/admin/" (version.key) } { (version.key) } }
							td { (version.version) }
							td { (version.published_at) }
							td { @if i == 0 { "latest" } }
						}
					}
				}
			}
		},
	}
	.render()
}
