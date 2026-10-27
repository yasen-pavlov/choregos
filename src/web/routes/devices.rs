//! `/devices`: the learned bus table, role → source mapping, identify, forget and raw send.

use askama::Template;
use axum::{extract::State, response::Html};

use crate::{
    state::Snapshot,
    web::{
        WebError, WebState,
        auth::Ctx,
        render,
        view::{self, Layout},
    },
};

#[derive(Template)]
#[template(path = "devices.html")]
struct DevicesTemplate {
    layout: Option<Layout>,
    snapshot: Snapshot,
}

pub(crate) async fn page(State(web): State<WebState>, ctx: Ctx) -> Result<Html<String>, WebError> {
    let snapshot = web.app.snapshot();
    render(&DevicesTemplate {
        layout: Some(Layout::new(ctx, "devices", &snapshot)),
        snapshot,
    })
}
