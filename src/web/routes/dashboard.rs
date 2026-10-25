//! `/`: mode, connectivity, role cards, Home Assistant states, conflicts.

use askama::Template;
use axum::{extract::State, response::Html};

use crate::{
    state::{HaEntityView, Snapshot},
    web::{
        WebError, WebState,
        auth::Ctx,
        render,
        view::{self, Layout},
    },
};

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    layout: Option<Layout>,
    snapshot: Snapshot,
    active_name: Option<String>,
    entities: Vec<(&'static str, HaEntityView)>,
}

pub(crate) async fn page(State(web): State<WebState>, ctx: Ctx) -> Result<Html<String>, WebError> {
    let snapshot = web.app.snapshot();
    let active_name = snapshot.active_role.as_ref().map(|active| {
        snapshot
            .roles
            .iter()
            .find(|role| &role.role == active)
            .map_or_else(|| active.clone(), |role| role.name.clone())
    });
    let mut entities = vec![
        ("tv", snapshot.ha.tv.clone()),
        ("receiver", snapshot.ha.receiver.clone()),
        ("sonos", snapshot.ha.sonos.clone()),
    ];
    entities.extend(
        snapshot
            .ha
            .media_players
            .iter()
            .map(|entity| ("media", entity.clone())),
    );
    render(&DashboardTemplate {
        layout: Some(Layout::new(ctx, "dashboard", &snapshot)),
        snapshot,
        active_name,
        entities,
    })
}
