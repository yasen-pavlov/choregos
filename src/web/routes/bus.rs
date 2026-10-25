//! `/bus`: frames interleaved with decisions, filterable, live via SSE.

use std::convert::Infallible;

use askama::Template;
use axum::{
    extract::{Query, State},
    response::{
        Html,
        sse::{Event, KeepAlive, Sse},
    },
};
use chrono::{DateTime, Utc};
use futures_util::{Stream, stream};
use serde::Deserialize;
use tokio::sync::broadcast::{Receiver, error::RecvError};

use crate::{
    state::{AppState, DecisionView, FrameView, Snapshot, UiEvent},
    web::{
        WebError, WebState,
        auth::Ctx,
        render,
        view::{self, Layout},
    },
};

/// Raw query; empty form fields arrive as empty strings.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct FilterQuery {
    #[serde(default)]
    la: String,
    #[serde(default)]
    opcode: String,
    #[serde(default)]
    hide_decisions: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Filter {
    la: Option<u8>,
    opcode: Option<u8>,
    hide_decisions: bool,
}

impl Filter {
    fn parse(query: &FilterQuery) -> Result<Self, WebError> {
        let la = match query.la.trim() {
            "" => None,
            text => Some(
                text.parse::<u8>()
                    .ok()
                    .filter(|la| *la <= 15)
                    .ok_or_else(|| {
                        WebError::BadRequest("LA filter must be a number 0–15".to_owned())
                    })?,
            ),
        };
        let opcode = match query.opcode.trim() {
            "" => None,
            text => {
                let digits = text
                    .strip_prefix("0x")
                    .or_else(|| text.strip_prefix("0X"))
                    .unwrap_or(text);
                Some(u8::from_str_radix(digits, 16).map_err(|_| {
                    WebError::BadRequest("opcode filter must be one hex byte".to_owned())
                })?)
            }
        };
        Ok(Self {
            la,
            opcode,
            hide_decisions: query.hide_decisions.is_some(),
        })
    }

    fn frame(&self, frame: &FrameView) -> bool {
        self.la.is_none_or(|la| frame.from == la || frame.to == la)
            && self
                .opcode
                .is_none_or(|opcode| frame.opcode == Some(opcode))
    }

    fn la_text(&self) -> String {
        self.la.map(|la| la.to_string()).unwrap_or_default()
    }

    fn opcode_text(&self) -> String {
        self.opcode
            .map(|opcode| format!("{opcode:02X}"))
            .unwrap_or_default()
    }

    /// Query string for the SSE endpoint so the live stream honours the same filter.
    fn query(&self) -> String {
        let mut parts = Vec::new();
        if let Some(la) = self.la {
            parts.push(format!("la={la}"));
        }
        if let Some(opcode) = self.opcode {
            parts.push(format!("opcode={opcode:02X}"));
        }
        if self.hide_decisions {
            parts.push("hide_decisions=on".to_owned());
        }
        parts.join("&")
    }
}

pub(crate) enum BusRow {
    Frame(FrameView),
    Decision(DecisionView),
}

impl BusRow {
    fn timestamp(&self) -> DateTime<Utc> {
        match self {
            Self::Frame(frame) => frame.timestamp,
            Self::Decision(decision) => decision.timestamp,
        }
    }
}

#[derive(Template)]
#[template(path = "bus.html")]
struct BusTemplate {
    layout: Option<Layout>,
    rows: Vec<BusRow>,
    filter: Filter,
    stream_url: String,
}

/// The whole table body; sent as the `reset` event when the stream (re)connects.
#[derive(Template)]
#[template(path = "partials/bus_rows.html")]
struct BusRowsTemplate {
    rows: Vec<BusRow>,
}

#[derive(Template)]
#[template(path = "partials/bus_row.html")]
struct BusRowTemplate {
    row: BusRow,
}

/// Filtered rows of the bounded history, newest first, and the newest timestamp it covers.
fn history(snapshot: &Snapshot, filter: Filter) -> (Vec<BusRow>, Option<DateTime<Utc>>) {
    let newest = snapshot
        .recent_frames
        .iter()
        .map(|frame| frame.timestamp)
        .chain(snapshot.decisions.iter().map(|decision| decision.timestamp))
        .max();
    let mut rows: Vec<BusRow> = if filter.hide_decisions {
        Vec::new()
    } else {
        snapshot
            .decisions
            .iter()
            .cloned()
            .map(BusRow::Decision)
            .collect()
    };
    rows.extend(
        snapshot
            .recent_frames
            .iter()
            .filter(|frame| filter.frame(frame))
            .cloned()
            .map(BusRow::Frame),
    );
    // Newest first, matching the `afterbegin` swap of live rows. The sort is stable,
    // so at equal timestamps a decision stays above the frame that caused it.
    rows.sort_by_key(|row| std::cmp::Reverse(row.timestamp()));
    (rows, newest)
}

pub(crate) async fn page(
    State(web): State<WebState>,
    ctx: Ctx,
    Query(query): Query<FilterQuery>,
) -> Result<Html<String>, WebError> {
    let filter = Filter::parse(&query)?;
    let snapshot = web.app.snapshot();
    let (rows, _) = history(&snapshot, filter);
    let stream_url = match filter.query() {
        query if query.is_empty() => "/bus/events".to_owned(),
        query => format!("/bus/events?{query}"),
    };
    render(&BusTemplate {
        layout: Some(Layout::new(ctx, "bus", &snapshot)),
        rows,
        filter,
        stream_url,
    })
}

/// One client's live feed. Every (re)connect, and every time the subscriber lags, starts by
/// replacing the table from the bounded snapshot, so gaps never go unnoticed or unfilled.
struct Feed {
    app: AppState,
    events: Receiver<UiEvent>,
    filter: Filter,
    /// Newest event already shown by the last reset; older broadcasts are duplicates.
    covered: Option<DateTime<Utc>>,
    resync: bool,
}

impl Feed {
    fn new(app: AppState, filter: Filter) -> Self {
        // Subscribe before the snapshot is read, so nothing falls between them.
        let events = app.subscribe();
        Self {
            app,
            events,
            filter,
            covered: None,
            resync: true,
        }
    }

    fn reset(&mut self) -> Result<Event, askama::Error> {
        let (rows, newest) = history(&self.app.snapshot(), self.filter);
        self.covered = self.covered.max(newest);
        let html = BusRowsTemplate { rows }.render()?;
        Ok(Event::default().event("reset").data(html))
    }

    async fn next(&mut self) -> Option<Event> {
        loop {
            let rendered = if self.resync {
                self.resync = false;
                self.reset()
            } else {
                let row = match self.events.recv().await {
                    Ok(UiEvent::Frame(frame)) => BusRow::Frame(frame),
                    Ok(UiEvent::Decision(decision)) => BusRow::Decision(decision),
                    Err(RecvError::Lagged(_)) => {
                        self.resync = true;
                        continue;
                    }
                    Err(RecvError::Closed) => return None,
                };
                let shown = match &row {
                    BusRow::Frame(frame) => self.filter.frame(frame),
                    BusRow::Decision(_) => !self.filter.hide_decisions,
                };
                if !shown
                    || self
                        .covered
                        .is_some_and(|covered| row.timestamp() <= covered)
                {
                    continue;
                }
                BusRowTemplate { row }
                    .render()
                    .map(|html| Event::default().event("row").data(html))
            };
            match rendered {
                Ok(event) => return Some(event),
                Err(error) => tracing::error!(%error, "cannot render bus rows"),
            }
        }
    }
}

pub(crate) async fn events(
    State(web): State<WebState>,
    Query(query): Query<FilterQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, WebError> {
    let filter = Filter::parse(&query)?;
    let rows = stream::unfold(Feed::new(web.app, filter), |mut feed| async move {
        feed.next().await.map(|event| (Ok(event), feed))
    });
    Ok(Sse::new(rows).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(la: &str, opcode: &str) -> FilterQuery {
        FilterQuery {
            la: la.to_owned(),
            opcode: opcode.to_owned(),
            hide_decisions: None,
        }
    }

    #[test]
    fn filter_parses_blank_decimal_la_and_hex_opcode() {
        assert_eq!(Filter::parse(&query(" ", "")).unwrap(), Filter::default());
        let filter = Filter::parse(&query("15", "0x8f")).unwrap();
        assert_eq!((filter.la, filter.opcode), (Some(15), Some(0x8F)));
        assert_eq!(filter.query(), "la=15&opcode=8F");
        assert!(Filter::parse(&query("16", "")).is_err());
        assert!(Filter::parse(&query("", "zz")).is_err());
        assert!(Filter::parse(&query("", "100")).is_err());
    }
}
