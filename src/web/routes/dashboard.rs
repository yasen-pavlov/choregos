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
    entities: Vec<HaRow>,
}

struct HaRow {
    kind: &'static str,
    entity: HaEntityView,
    /// The TV drops off the network ~2 min after standby, so Home Assistant reports it
    /// `unavailable`/`unknown` whenever the room is off: render that as a muted `off`, not
    /// a fault. Every other entity keeps its raw state (and the error styling).
    off_unreachable: bool,
}

impl HaRow {
    fn new(kind: &'static str, entity: &HaEntityView) -> Self {
        Self {
            kind,
            entity: entity.clone(),
            off_unreachable: false,
        }
    }

    fn tv(entity: &HaEntityView) -> Self {
        Self {
            off_unreachable: matches!(entity.state.as_str(), "unavailable" | "unknown"),
            ..Self::new("tv", entity)
        }
    }
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
    let ha = &snapshot.ha;
    let entities = [
        HaRow::tv(&ha.tv),
        HaRow::new("receiver", &ha.receiver),
        HaRow::new("sonos", &ha.sonos),
    ]
    .into_iter()
    .chain(
        ha.media_players
            .iter()
            .map(|entity| HaRow::new("media", entity)),
    )
    .collect();
    render(&DashboardTemplate {
        layout: Some(Layout::new(ctx, "dashboard", &snapshot)),
        snapshot,
        active_name,
        entities,
    })
}
