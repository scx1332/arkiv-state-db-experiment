//! Same-origin Scalar reference; independent of database locks and job permits.
use axum::{Json, response::Html};
use serde_json::Value;
use std::sync::LazyLock;

const SPEC: &str = include_str!("state-openapi.json");
static DOCUMENT: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(SPEC).expect("embedded OpenAPI document must be valid"));

pub(super) async fn html() -> Html<&'static str> {
    Html(include_str!("state-docs.html"))
}

pub(super) async fn openapi() -> Json<Value> {
    Json(DOCUMENT.clone())
}
