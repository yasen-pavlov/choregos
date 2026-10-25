//! `/decisions`: the decision log, newest first, including shadow-mode `would:` entries.

use askama::Template;
use axum::{extract::State, response::Html};

use crate::{
    state::DecisionView,
    web::{
        WebError, WebState,
        auth::Ctx,
        render,
        view::{self, Layout},
    },
};

#[derive(Template)]
#[template(path = "decisions.html")]
struct DecisionsTemplate {
    layout: Option<Layout>,
    decisions: Vec<DecisionView>,
}

pub(crate) async fn page(State(web): State<WebState>, ctx: Ctx) -> Result<Html<String>, WebError> {
    let snapshot = web.app.snapshot();
    let layout = Layout::new(ctx, "decisions", &snapshot);
    let mut decisions: Vec<DecisionView> = snapshot.decisions.into();
    decisions.sort_by_key(|decision| std::cmp::Reverse(decision.timestamp));
    render(&DecisionsTemplate {
        layout: Some(layout),
        decisions,
    })
}
