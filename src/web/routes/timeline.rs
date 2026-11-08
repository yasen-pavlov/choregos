//! `/recorder/timeline`: one window of the bus as lanes (room, decisions, one per logical
//! address, device observations), zoomed and panned through query parameters; the
//! `#timeline` partial (which the recorder script's live tail also re-fetches), the
//! selected-decision panel, and the same window as JSON.

use askama::Template;
use axum::{
    Json,
    extract::{Path, Query, State},
    response::Html,
};
use chrono::{DateTime, Utc};
use openidconnect::url::form_urlencoded;
use serde::Deserialize;

use super::recorder::{Read, Source, read};
use crate::{
    action::Trigger,
    state::{DecisionView, Snapshot},
    web::{
        WebError, WebState,
        auth::Ctx,
        render,
        view::{
            self, ActionSummary, Layout,
            recorder::{ExpectationRow, expectation_rows, headline, trigger_compact},
            timeline::{self, Lanes, MAX_SPAN, Span, WindowJson, Zoom},
            window::Entry,
        },
    },
};

/// The most entries a window carries, like the bus window's trace read.
const MAX_ENTRIES: usize = 5000;

/// `at` (the window's centre, RFC 3339, or `now`), `zoom` and the selected decision.
/// Without `at` the window centres on the selection, else it ends now (the live edge);
/// `at=now` ends it now even with a selection (the live tail's re-fetch).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct TimelineQuery {
    at: Option<String>,
    zoom: Option<String>,
    select: Option<String>,
}

/// The parsed query.
struct Position {
    /// `None`: the window ends now.
    at: Option<DateTime<Utc>>,
    zoom: Zoom,
    /// `at=now`: stay on the live edge, even with a selection.
    edge: bool,
}

impl Position {
    fn parse(query: &TimelineQuery) -> Result<Self, WebError> {
        let zoom = match query.zoom.as_deref().filter(|zoom| !zoom.is_empty()) {
            None => Zoom::M5,
            Some(word) => Zoom::parse(word).ok_or_else(|| {
                WebError::BadRequest("zoom is one of 1m, 5m, 15m and 1h".to_owned())
            })?,
        };
        let (at, edge) = match query.at.as_deref().filter(|at| !at.is_empty()) {
            None => (None, false),
            Some("now") => (None, true),
            Some(text) => (
                Some(
                    DateTime::parse_from_rfc3339(text)
                        .map_err(|_| {
                            WebError::BadRequest("at is an RFC 3339 time or now".to_owned())
                        })?
                        .with_timezone(&Utc),
                ),
                false,
            ),
        };
        Ok(Self { at, zoom, edge })
    }

    fn span(&self, now: DateTime<Utc>) -> Span {
        match self.at {
            Some(at) => Span::around(at, self.zoom),
            None => Span {
                from: now - self.zoom.span(),
                to: now,
            },
        }
    }
}

/// `at=…&zoom=…&select=…` (each only when set). A selection on the live edge keeps the
/// edge with `at=now` (a bare selection would centre on it).
fn query(at: Option<DateTime<Utc>>, zoom: Zoom, select: Option<u64>) -> String {
    let mut query = form_urlencoded::Serializer::new(String::new());
    match (at, select) {
        (Some(at), _) => {
            query.append_pair("at", &view::iso(&at));
        }
        (None, Some(_)) => {
            query.append_pair("at", "now");
        }
        (None, None) => {}
    }
    query.append_pair("zoom", zoom.word());
    if let Some(id) = select {
        query.append_pair("select", &id.to_string());
    }
    query.finish()
}

/// A toolbar link: the page URL (pushed to history) and the partial it swaps in.
struct NavLink {
    label: String,
    page: String,
    partial: String,
    zoom: &'static str,
    current: bool,
}

impl NavLink {
    fn new(label: String, at: Option<DateTime<Utc>>, zoom: Zoom, select: Option<u64>) -> Self {
        let query = query(at, zoom, select);
        Self {
            label,
            page: format!("/recorder/timeline?{query}"),
            partial: format!("/p/timeline?{query}"),
            zoom: zoom.word(),
            current: false,
        }
    }
}

/// The side panel's decision.
struct SelectedView {
    decision: DecisionView,
    headline: String,
    /// `17:08:40.860 · ReportPhysicalAddress`.
    when: String,
    /// `11 → 15 84 23 00 04`, or the compact non-frame trigger.
    trigger: String,
    expectations: Vec<ExpectationRow>,
    /// `TV · 1 HA`, `—`.
    actions: String,
    children: Vec<u64>,
    /// Closes the panel: the same window without the selection.
    close: NavLink,
}

impl SelectedView {
    fn new(decision: DecisionView, snapshot: &Snapshot, close: NavLink) -> Self {
        let trigger = match &decision.trigger {
            Trigger::Frame { from, to, data } => {
                let bytes =
                    hex::decode(data).map_or_else(|_| data.clone(), |b| view::hex_bytes(&b));
                format!("{from} → {to} {bytes}")
            }
            other => trigger_compact(other),
        };
        Self {
            headline: headline(&decision, snapshot),
            when: format!("{} · {}", view::clock(&decision.timestamp), decision.reason),
            trigger,
            expectations: expectation_rows(&decision, snapshot),
            actions: ActionSummary::from_actions(&decision.actions).text(),
            children: snapshot
                .decisions
                .iter()
                .filter(|row| row.parent == Some(decision.id))
                .map(|row| row.id)
                .collect(),
            close,
            decision,
        }
    }
}

/// The `#timeline` block: toolbar, lanes, legend and the selected-decision panel.
struct Timeline {
    span: Span,
    zoom: Zoom,
    /// The window's centre when it was asked for; `None` at the live edge.
    at: Option<DateTime<Utc>>,
    bridge: u8,
    select: Option<u64>,
    lanes: Lanes,
    source: String,
    /// Drawn frame ticks and decision markers.
    frames: usize,
    decisions: usize,
    truncated: bool,
    zooms: Vec<NavLink>,
    prev: NavLink,
    /// `None` when the window already ends now.
    next: Option<NavLink>,
    selected: Option<SelectedView>,
}

#[derive(Template)]
#[template(path = "partials/timeline.html")]
struct TimelinePartial {
    timeline: Timeline,
}

#[derive(Template)]
#[template(path = "timeline.html")]
struct TimelinePage {
    layout: Option<Layout>,
    timeline: Timeline,
}

#[derive(Template)]
#[template(path = "partials/timeline_selected.html")]
struct SelectedPartial {
    selected: Option<SelectedView>,
}

fn ring_row(snapshot: &Snapshot, id: &str) -> Result<DecisionView, WebError> {
    let id: u64 = id
        .parse()
        .map_err(|_| WebError::BadRequest("select is a decision id".to_owned()))?;
    snapshot
        .decisions
        .iter()
        .find(|decision| decision.id == id)
        .cloned()
        .ok_or(WebError::NotFound)
}

/// A window read, tagged and laid out.
struct Laid {
    entries: Vec<Entry>,
    lanes: Lanes,
    source: Source,
    truncated: bool,
}

async fn lay_out(
    web: &WebState,
    snapshot: &Snapshot,
    span: Span,
    now: DateTime<Utc>,
    selected: Option<&DecisionView>,
) -> Laid {
    let Read {
        mut entries,
        source,
        mut truncated,
        ..
    } = read(web, snapshot, None, span.from, span.to).await;
    if entries.len() > MAX_ENTRIES {
        entries.truncate(MAX_ENTRIES);
        truncated = true;
    }
    let triggers = timeline::tag_and_settle(&mut entries, snapshot);
    let memory = matches!(source, Source::Memory);
    let lanes = timeline::lanes(snapshot, &entries, &triggers, span, now, selected, memory);
    Laid {
        entries,
        lanes,
        source,
        truncated,
    }
}

async fn partial(
    web: &WebState,
    snapshot: &Snapshot,
    query: &TimelineQuery,
) -> Result<Timeline, WebError> {
    let mut position = Position::parse(query)?;
    let selected = match query.select.as_deref().filter(|id| !id.is_empty()) {
        Some(id) => Some(ring_row(snapshot, id)?),
        None => None,
    };
    if position.at.is_none() && !position.edge {
        position.at = selected.as_ref().map(|decision| decision.timestamp);
    }
    let now = Utc::now();
    let span = position.span(now);
    let Laid {
        lanes,
        source,
        truncated,
        ..
    } = lay_out(web, snapshot, span, now, selected.as_ref()).await;
    let select = selected.as_ref().map(|decision| decision.id);
    let zoom = position.zoom;
    let at = position.at;
    let centre = at.unwrap_or(span.from + zoom.span() / 2);
    let step = zoom.step_label();
    Ok(Timeline {
        frames: lanes.addresses.iter().map(|lane| lane.ticks.len()).sum(),
        decisions: lanes.decisions.len(),
        zooms: Zoom::ALL
            .into_iter()
            .map(|other| NavLink {
                current: other == zoom,
                ..NavLink::new(other.label().to_owned(), at, other, select)
            })
            .collect(),
        prev: NavLink::new(
            format!("← {step}"),
            Some(centre - zoom.step()),
            zoom,
            select,
        ),
        next: (span.to < now).then(|| {
            NavLink::new(
                format!("{step} →"),
                Some(centre + zoom.step()),
                zoom,
                select,
            )
        }),
        selected: selected.map(|decision| {
            let close = NavLink::new("✕".to_owned(), at, zoom, None);
            SelectedView::new(decision, snapshot, close)
        }),
        span,
        zoom,
        at,
        bridge: snapshot.bridge_logical_address,
        select,
        lanes,
        source: source.words(),
        truncated,
    })
}

pub(crate) async fn page(
    State(web): State<WebState>,
    ctx: Ctx,
    Query(query): Query<TimelineQuery>,
) -> Result<Html<String>, WebError> {
    let snapshot = web.app.snapshot();
    let timeline = partial(&web, &snapshot, &query).await?;
    render(&TimelinePage {
        layout: Some(Layout::new(ctx, "recorder", &snapshot)),
        timeline,
    })
}

/// The `#timeline` block of the same query, for the zoom and pan links and the script.
pub(crate) async fn block(
    State(web): State<WebState>,
    Query(query): Query<TimelineQuery>,
) -> Result<Html<String>, WebError> {
    let snapshot = web.app.snapshot();
    render(&TimelinePartial {
        timeline: partial(&web, &snapshot, &query).await?,
    })
}

/// The `#selected` panel for one decision; `at` and `zoom` only shape its close link.
pub(crate) async fn selected(
    State(web): State<WebState>,
    Path(id): Path<String>,
    Query(query): Query<TimelineQuery>,
) -> Result<Html<String>, WebError> {
    let snapshot = web.app.snapshot();
    let position = Position::parse(&query)?;
    let id: u64 = id.parse().map_err(|_| WebError::NotFound)?;
    let decision = snapshot
        .decisions
        .iter()
        .find(|decision| decision.id == id)
        .cloned()
        .ok_or(WebError::NotFound)?;
    let close = NavLink::new("✕".to_owned(), position.at, position.zoom, None);
    render(&SelectedPartial {
        selected: Some(SelectedView::new(decision, &snapshot, close)),
    })
}

/// `from` and `to` (RFC 3339) of a JSON window.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct WindowJsonQuery {
    from: Option<String>,
    to: Option<String>,
}

fn instant(text: Option<&str>, name: &str) -> Result<DateTime<Utc>, WebError> {
    text.and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|at| at.with_timezone(&Utc))
        .ok_or_else(|| WebError::BadRequest(format!("{name} is an RFC 3339 time")))
}

/// The window the partial renders, as JSON: its entries (frames with tags, observations,
/// decisions) and lanes. At most an hour and 5000 entries.
pub(crate) async fn window_json(
    State(web): State<WebState>,
    Query(query): Query<WindowJsonQuery>,
) -> Result<Json<WindowJson>, WebError> {
    let from = instant(query.from.as_deref(), "from")?;
    let to = instant(query.to.as_deref(), "to")?;
    if to <= from || to - from > MAX_SPAN {
        return Err(WebError::BadRequest(
            "the window must satisfy from < to ≤ from + 1 h".to_owned(),
        ));
    }
    let snapshot = web.app.snapshot();
    let span = Span { from, to };
    let Laid {
        entries,
        lanes,
        source,
        truncated,
    } = lay_out(&web, &snapshot, span, Utc::now(), None).await;
    Ok(Json(WindowJson {
        from,
        to,
        source: source.words(),
        truncated,
        entries,
        lanes,
    }))
}
