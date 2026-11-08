//! `/recorder`: the decision list, one decision with its bus window, and the reason
//! glossary. The window reads the trace day files when capture is on and the day's file
//! exists, else the frames and decisions held in memory.

use std::path::PathBuf;

use askama::Template;
use axum::{
    extract::{Path, Query, State},
    response::Html,
};
use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use serde::Deserialize;

use crate::{
    action::Verdict,
    config::Config,
    runtime::trace,
    state::{DecisionView, Snapshot},
    web::{
        WebError, WebState,
        auth::Ctx,
        render,
        view::{
            self, Layout, ReasonGroup,
            recorder::{
                self as rec, ActionRow, ExpectationRow, GROUPS, ListFilter, ListItem, Range,
                TriggerView, VERDICTS, WouldFilter,
            },
            window::{self, Entry, Item},
        },
    },
};

/// The default window around a decision, and the widened one (seconds).
const NARROW: (i64, i64) = (-5, 45);
const WIDE: (i64, i64) = (-30, 120);

// ---- list ---------------------------------------------------------------------------------

struct List {
    filter: ListFilter,
    items: Vec<ListItem>,
    total: usize,
    ring: usize,
    /// Rows partial polled while on the first page.
    poll_url: Option<String>,
    older_url: Option<String>,
    newest_url: Option<String>,
    roles: Vec<(String, String)>,
    groups: [ReasonGroup; 9],
    verdicts: [Verdict; 6],
    ranges: [Range; 5],
    woulds: [WouldFilter; 3],
}

impl List {
    fn new(snapshot: &Snapshot, filter: ListFilter, now: DateTime<Utc>) -> Self {
        let page = filter.page(snapshot.decisions.iter(), now);
        let items = rec::list_items(&page.rows, snapshot, now.date_naive());
        Self {
            total: page.total,
            ring: snapshot.decisions.len(),
            poll_url: filter.before.is_none().then(|| filter.rows_url()),
            older_url: page.older.map(|id| filter.url(Some(id))),
            newest_url: filter.before.is_some().then(|| filter.url(None)),
            roles: snapshot
                .roles
                .iter()
                .map(|role| (role.role.clone(), role.name.clone()))
                .collect(),
            items,
            filter,
            groups: GROUPS,
            verdicts: VERDICTS,
            ranges: Range::ALL,
            woulds: WouldFilter::ALL,
        }
    }

    fn range_is(&self, range: &Range) -> bool {
        self.filter.range == *range
    }

    fn would_is(&self, would: &WouldFilter) -> bool {
        self.filter.would == *would
    }
}

#[derive(Template)]
#[template(path = "recorder.html")]
struct ListPage {
    layout: Option<Layout>,
    list: List,
}

#[derive(Template)]
#[template(path = "partials/recorder_results.html")]
struct ResultsPartial {
    list: List,
}

fn filter(pairs: &[(String, String)]) -> Result<ListFilter, WebError> {
    ListFilter::parse(pairs).map_err(WebError::BadRequest)
}

pub(crate) async fn page(
    State(web): State<WebState>,
    ctx: Ctx,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Result<Html<String>, WebError> {
    let filter = filter(&pairs)?;
    let snapshot = web.app.snapshot();
    render(&ListPage {
        layout: Some(Layout::new(ctx, "recorder", &snapshot)),
        list: List::new(&snapshot, filter, Utc::now()),
    })
}

/// The `#recorder-results` block (rows, count and pager, all from one snapshot) of the
/// same query, polled by the first page.
pub(crate) async fn rows(
    State(web): State<WebState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Result<Html<String>, WebError> {
    let filter = filter(&pairs)?;
    render(&ResultsPartial {
        list: List::new(&web.app.snapshot(), filter, Utc::now()),
    })
}

// ---- detail -------------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "decision.html")]
struct DetailPage {
    layout: Option<Layout>,
    decision: DecisionView,
    group: ReasonGroup,
    headline: String,
    /// `17:08:40.860 · today · role console_a off → on · mode active`.
    when: String,
    met: Option<String>,
    prev: Option<u64>,
    next: Option<u64>,
    trigger: TriggerView,
    sentence: Option<&'static str>,
    rule: &'static str,
    actions: Vec<ActionRow>,
    expectations: Vec<ExpectationRow>,
    parent: Option<DecisionView>,
    children: Vec<DecisionView>,
    role_name: Option<String>,
    window_url: String,
}

fn ring_row(snapshot: &Snapshot, id: &str) -> Result<DecisionView, WebError> {
    let id: u64 = id.parse().map_err(|_| WebError::NotFound)?;
    snapshot
        .decisions
        .iter()
        .find(|decision| decision.id == id)
        .cloned()
        .ok_or(WebError::NotFound)
}

/// `today`, `yesterday`, or the UTC date.
fn day_word(date: NaiveDate, today: NaiveDate) -> String {
    match (today - date).num_days() {
        0 => "today".to_owned(),
        1 => "yesterday".to_owned(),
        _ => date.format("%a %-d %b").to_string(),
    }
}

fn when_line(decision: &DecisionView, now: DateTime<Utc>) -> String {
    let mut parts = vec![
        view::clock(&decision.timestamp),
        day_word(decision.timestamp.date_naive(), now.date_naive()),
    ];
    if let Some(role) = &decision.role {
        let change = view::transition(&decision.from, &decision.to);
        parts.push(format!("role {role} {change}").trim_end().to_owned());
    }
    if decision.would {
        parts.push("recorded as would".to_owned());
    } else if decision.reason != "service_started" {
        parts.push("mode active".to_owned());
    }
    parts.join(" · ")
}

fn window_url(id: u64, (from, to): (i64, i64)) -> String {
    format!("/p/window/{id}?from={from}&to={to}")
}

pub(crate) async fn detail(
    State(web): State<WebState>,
    ctx: Ctx,
    Path(id): Path<String>,
) -> Result<Html<String>, WebError> {
    let snapshot = web.app.snapshot();
    let decision = ring_row(&snapshot, &id)?;
    let now = Utc::now();
    let present =
        |id: Option<u64>| id.filter(|id| snapshot.decisions.iter().any(|row| row.id == *id));
    let group = view::reason_group(&decision.reason);
    render(&DetailPage {
        layout: Some(Layout::new(ctx, "recorder", &snapshot)),
        group,
        headline: rec::headline(&decision, &snapshot),
        when: when_line(&decision, now),
        met: rec::met_summary(&decision),
        prev: present(decision.id.checked_sub(1)),
        next: present(decision.id.checked_add(1)),
        trigger: rec::trigger_view(&decision.trigger, &snapshot),
        sentence: view::reason_sentence(&decision.reason),
        rule: rec::rule_sentence(group),
        actions: rec::action_rows(&decision, &snapshot),
        expectations: rec::expectation_rows(&decision, &snapshot),
        parent: decision.parent.and_then(|parent| {
            snapshot
                .decisions
                .iter()
                .find(|row| row.id == parent)
                .cloned()
        }),
        children: snapshot
            .decisions
            .iter()
            .filter(|row| row.parent == Some(decision.id))
            .cloned()
            .collect(),
        role_name: decision
            .role
            .as_deref()
            .map(|role| view::role_name(&snapshot, role)),
        window_url: window_url(decision.id, NARROW),
        decision,
    })
}

// ---- bus window ---------------------------------------------------------------------------

/// Window bounds in seconds around the decision and the row toggles. A form submit sets
/// `set`, so an unticked box (absent) hides; without `set` both are shown.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct WindowQuery {
    from: Option<i64>,
    to: Option<i64>,
    polls: Option<String>,
    obs: Option<String>,
    set: Option<String>,
}

pub(super) enum Source {
    Trace(Vec<String>),
    Memory,
}

impl Source {
    /// `memory`, `the trace file x`, `the trace files x and y`.
    pub(super) fn words(&self) -> String {
        match self {
            Self::Trace(files) => format!(
                "the trace file{} {}",
                if files.len() > 1 { "s" } else { "" },
                files.join(" and ")
            ),
            Self::Memory => "memory".to_owned(),
        }
    }
}

/// A window's entries in time order, where they came from, whether the trace read hit its
/// cap, and the trace retention it was read under.
pub(super) struct Read {
    pub(super) entries: Vec<Entry>,
    pub(super) source: Source,
    pub(super) truncated: bool,
    pub(super) keep_days: u32,
}

/// The window `[from, to]` from the trace day files that cover it, else from memory.
/// `focus` is the decision the window is about, if any (it is always a row).
pub(super) async fn read(
    web: &WebState,
    snapshot: &Snapshot,
    focus: Option<&DecisionView>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Read {
    let keep_days =
        Config::parse(&web.app.config_text()).map_or(0, |config| config.trace.keep_days);
    if let Some((directory, files)) = trace_files(web.app.trace_directory(), keep_days, from, to) {
        match tokio::task::spawn_blocking(move || {
            trace::read_window(&directory, from, to, trace::ReadBudget::WINDOW)
        })
        .await
        {
            Ok(Ok(read)) => {
                return Read {
                    entries: window::from_trace(read.entries, snapshot, focus),
                    source: Source::Trace(files),
                    truncated: read.truncated,
                    keep_days,
                };
            }
            Ok(Err(error)) => tracing::warn!(%error, "cannot read the bus trace; using memory"),
            Err(error) => tracing::warn!(%error, "trace reader failed; using memory"),
        }
    }
    Read {
        entries: window::from_memory(snapshot, focus, from, to),
        source: Source::Memory,
        truncated: false,
        keep_days,
    }
}

#[derive(Template)]
#[template(path = "partials/window.html")]
struct WindowPartial {
    id: u64,
    from_s: i64,
    to_s: i64,
    entries: Vec<Entry>,
    polls: bool,
    obs: bool,
    /// (label, url) of the other window size.
    resize: (&'static str, String),
    footer: String,
    truncated: bool,
}

/// `−5 s`, `+2 m`.
fn offset(seconds: i64) -> String {
    let sign = if seconds < 0 { "−" } else { "+" };
    let magnitude = seconds.unsigned_abs();
    if magnitude >= 60 && magnitude.is_multiple_of(60) {
        format!("{sign}{} m", magnitude / 60)
    } else {
        format!("{sign}{magnitude} s")
    }
}

impl WindowPartial {
    /// `−5 s → +45 s`.
    fn span(&self) -> String {
        format!("{} → {}", offset(self.from_s), offset(self.to_s))
    }
}

/// The trace day files that exist for the window's UTC dates, when capture is on.
fn trace_files(
    directory: Option<PathBuf>,
    keep_days: u32,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Option<(PathBuf, Vec<String>)> {
    let directory = directory.filter(|_| keep_days > 0)?;
    let files: Vec<String> = from
        .date_naive()
        .iter_days()
        .take_while(|date| *date <= to.date_naive())
        .map(|date| trace::day_file(&directory, date))
        .filter(|path| path.is_file())
        .filter_map(|path| Some(path.file_name()?.to_string_lossy().into_owned()))
        .collect();
    (!files.is_empty()).then_some((directory, files))
}

pub(crate) async fn window(
    State(web): State<WebState>,
    Path(id): Path<String>,
    Query(query): Query<WindowQuery>,
) -> Result<Html<String>, WebError> {
    let snapshot = web.app.snapshot();
    let decision = ring_row(&snapshot, &id)?;
    let (from_s, to_s) = (query.from.unwrap_or(NARROW.0), query.to.unwrap_or(NARROW.1));
    if !(-300..=0).contains(&from_s) || !(0..=600).contains(&to_s) {
        return Err(WebError::BadRequest(
            "the window must satisfy -300 ≤ from ≤ 0 ≤ to ≤ 600 (seconds)".to_owned(),
        ));
    }
    let from = decision.timestamp + TimeDelta::seconds(from_s);
    let to = decision.timestamp + TimeDelta::seconds(to_s);
    let Read {
        mut entries,
        source,
        truncated,
        keep_days,
    } = read(&web, &snapshot, Some(&decision), from, to).await;
    window::tag(&mut entries, &decision, snapshot.bridge_logical_address);
    window::settle(&mut entries, &decision, &snapshot);
    let frames = entries
        .iter()
        .filter(|entry| matches!(entry.item, Item::Frame(_)))
        .count();
    let polls = query.set.is_none() || query.polls.is_some();
    let obs = query.set.is_none() || query.obs.is_some();
    entries.retain(|entry| match &entry.item {
        Item::Frame(frame) => polls || !frame.is_poll(),
        Item::Obs(_) => obs,
        Item::Decision(_) => true,
    });
    let footer = match source {
        Source::Trace(_) => format!(
            "{} {}.",
            if frames == 0 {
                "No frames for this window in"
            } else {
                "Frames from"
            },
            source.words()
        ),
        Source::Memory if frames == 0 => format!(
            "No bus record for this window: it is not in memory (last ~6 h) and there is no trace file for that day (trace kept {keep_days} days)."
        ),
        Source::Memory => "Frames from memory (last ~6 h). Memory holds no observations; they are only in the trace files.".to_owned(),
    };
    let resize = if (from_s, to_s) == NARROW {
        ("Widen to −30 s / +2 m", window_url(decision.id, WIDE))
    } else {
        ("Back to −5 s / +45 s", window_url(decision.id, NARROW))
    };
    render(&WindowPartial {
        id: decision.id,
        from_s,
        to_s,
        entries,
        polls,
        obs,
        resize,
        footer,
        truncated,
    })
}

// ---- glossary -----------------------------------------------------------------------------

/// One group of the glossary: its rule and its reason literals with their sentences.
struct GlossaryGroup {
    group: ReasonGroup,
    rule: &'static str,
    reasons: Vec<(&'static str, &'static str)>,
}

#[derive(Template)]
#[template(path = "glossary.html")]
struct GlossaryPage {
    layout: Option<Layout>,
    groups: Vec<GlossaryGroup>,
}

pub(crate) async fn glossary(
    State(web): State<WebState>,
    ctx: Ctx,
) -> Result<Html<String>, WebError> {
    let snapshot = web.app.snapshot();
    render(&GlossaryPage {
        layout: Some(Layout::new(ctx, "recorder", &snapshot)),
        groups: rec::glossary()
            .into_iter()
            .map(|(group, reasons)| GlossaryGroup {
                group,
                rule: rec::rule_sentence(group),
                reasons,
            })
            .collect(),
    })
}
