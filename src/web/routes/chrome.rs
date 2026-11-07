//! Chrome partials polled by every page: `/p/status` (top-bar status and bands) and
//! `/p/bridge` (the bridge drawer with raw send).

use askama::Template;
use axum::{
    extract::{Query, State},
    response::Html,
};
use serde::Deserialize;

use crate::{
    state::{Mode, Snapshot},
    web::{
        WebError, WebState, render,
        view::{self, Status},
    },
};

#[derive(Template)]
#[template(path = "partials/status_poll.html")]
struct StatusPartial {
    status: Status,
}

#[derive(Template)]
#[template(path = "partials/bridge.html")]
struct BridgeDrawer {
    snapshot: Snapshot,
    shadow: bool,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct BridgeQuery {
    /// The drawer's close button: answer with nothing so `#drawer` empties and polling stops.
    #[serde(default)]
    close: bool,
}

pub(crate) async fn status(State(web): State<WebState>) -> Result<Html<String>, WebError> {
    render(&StatusPartial {
        status: Status::new(&web.app.snapshot(), false),
    })
}

pub(crate) async fn bridge(
    State(web): State<WebState>,
    Query(query): Query<BridgeQuery>,
) -> Result<Html<String>, WebError> {
    if query.close {
        return Ok(Html(String::new()));
    }
    let snapshot = web.app.snapshot();
    render(&BridgeDrawer {
        shadow: snapshot.mode == Mode::Shadow,
        snapshot,
    })
}
