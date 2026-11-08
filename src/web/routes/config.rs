//! `/config`: section forms, the role editor and the raw editor, each ending in the same
//! review (line diff) → save step. Edits are applied to the file's own document
//! (`view::config`), so comments, ordering and formatting survive; secrets never appear.

use askama::Template;
use axum::{
    Form,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
};
use serde::Deserialize;
use toml_edit::DocumentMut;

use crate::{
    config::{Config, ConfigError},
    web::{
        WebError, WebState,
        auth::Ctx,
        render,
        view::{
            self, Layout, Status,
            config::{
                self as cv, Change, Checked, DiffLine, Disk, FormData, Gate, NavItem, SectionView,
            },
            devices::Part,
        },
    },
};

/// The raw editor.
pub(crate) struct RawView {
    pub(crate) text: String,
    /// `(line, message)`.
    pub(crate) problems: Vec<(Option<usize>, String)>,
    pub(crate) changes: usize,
    pub(crate) dirty: bool,
    /// Hash of the file when the editor opened; the review warns when it moved since.
    pub(crate) opened: String,
}

impl RawView {
    fn can_review(&self) -> bool {
        self.dirty && self.problems.is_empty()
    }
}

/// The review step shared by the form and the raw editor.
pub(crate) struct ReviewView {
    pub(crate) changes: Vec<Change>,
    pub(crate) diff: Vec<DiffLine>,
    /// Added and removed lines.
    pub(crate) diff_count: usize,
    pub(crate) proposed: String,
    pub(crate) base: String,
    /// The form (or raw text) the review came from, for "Back to edit" and a stale retry.
    pub(crate) carried: Vec<(String, String)>,
    pub(crate) section: Option<&'static str>,
    /// Why this review was redone: the file changed underneath.
    pub(crate) stale: Option<&'static str>,
}

/// After a save attempt.
pub(crate) struct SavedView {
    pub(crate) ok: bool,
    pub(crate) at: String,
    pub(crate) changes: usize,
    pub(crate) error: Option<String>,
    pub(crate) carried: Vec<(String, String)>,
    pub(crate) section: Option<&'static str>,
}

impl SavedView {
    fn tone(&self) -> &'static str {
        if self.ok { "ok" } else { "bad" }
    }
}

#[derive(Template)]
#[template(path = "config.html")]
struct ConfigPage {
    layout: Option<Layout>,
    nav: Vec<NavItem>,
    nav_oob: bool,
    section: Option<SectionView>,
    raw: Option<RawView>,
}

/// A section (form) swapped into `#section`, with the nav out of band.
#[derive(Template)]
#[template(path = "partials/config_swap.html")]
struct SectionSwap {
    s: SectionView,
    nav: Vec<NavItem>,
    nav_oob: bool,
}

/// Only the parts of a section that follow its values (field states, errors, row headers,
/// the footer, the nav), out of band: the inputs being typed into are left alone.
#[derive(Template)]
#[template(path = "partials/config_state.html")]
struct SectionState {
    s: SectionView,
    nav: Vec<NavItem>,
    nav_oob: bool,
}

#[derive(Template)]
#[template(path = "partials/config_raw.html")]
struct RawSwap {
    raw: RawView,
}

#[derive(Template)]
#[template(path = "partials/config_raw_state.html")]
struct RawState {
    raw: RawView,
}

#[derive(Template)]
#[template(path = "partials/config_review.html")]
struct ReviewSwap {
    review: ReviewView,
}

#[derive(Template)]
#[template(path = "partials/config_saved.html")]
struct SavedSwap {
    saved: SavedView,
    status: Status,
    /// Empty after a raw save (the raw tab has no section nav).
    nav: Vec<NavItem>,
    nav_oob: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PageQuery {
    section: Option<String>,
    tab: Option<String>,
}

/// The file as it is on disk now (read by the runtime): text, document and config (`None`
/// when the text does not validate, which only an edit outside the UI can cause; the form
/// then refuses edits).
struct Current {
    text: String,
    doc: DocumentMut,
    config: Option<Config>,
}

impl Current {
    async fn read(web: &WebState) -> Result<Self, WebError> {
        let text = web.app.read_config().await?;
        Ok(Self {
            doc: text.parse().unwrap_or_default(),
            config: Config::parse(&text).ok(),
            text,
        })
    }

    fn disk(&self) -> Disk<'_> {
        Disk {
            doc: &self.doc,
            text: &self.text,
            config: self.config.as_ref(),
        }
    }
}

fn section_key(key: &str) -> Result<(&'static str, Gate), WebError> {
    cv::section(key).ok_or(WebError::NotFound)
}

/// A section as the file has it, before any edit, showing `problems`.
fn fresh_section(
    web: &WebState,
    current: &Current,
    key: &'static str,
    gate: Gate,
    problems: Vec<String>,
) -> SectionView {
    let form = cv::fresh_form(&current.doc);
    let checked = Checked {
        config: current.config.clone(),
        problems,
        ..Checked::default()
    };
    cv::section_view(
        key,
        gate,
        &current.disk(),
        &form,
        &checked,
        &web.app.snapshot(),
    )
}

fn fresh_raw(current: &Current) -> RawView {
    RawView {
        text: current.text.clone(),
        problems: Vec::new(),
        changes: 0,
        dirty: false,
        opened: cv::hash(&current.text),
    }
}

pub(crate) async fn page(
    State(web): State<WebState>,
    ctx: Ctx,
    Query(query): Query<PageQuery>,
) -> Result<Html<String>, WebError> {
    let current = Current::read(&web).await?;
    let snapshot = web.app.snapshot();
    let key = match query.section.as_deref() {
        Some(key) => section_key(key)?.0,
        None => cv::default_section(&current.doc),
    };
    let (section, raw) = if query.tab.as_deref() == Some("raw") {
        (None, Some(fresh_raw(&current)))
    } else {
        let (key, gate) = section_key(key)?;
        (
            Some(fresh_section(&web, &current, key, gate, Vec::new())),
            None,
        )
    };
    render(&ConfigPage {
        layout: Some(Layout::new(ctx, "config", &snapshot)),
        nav: cv::nav(key, current.config.as_ref(), None),
        nav_oob: false,
        section,
        raw,
    })
}

pub(crate) async fn partial(
    State(web): State<WebState>,
    Path(key): Path<String>,
) -> Result<Html<String>, WebError> {
    let (key, gate) = section_key(&key)?;
    let current = Current::read(&web).await?;
    render(&SectionSwap {
        s: fresh_section(&web, &current, key, gate, Vec::new()),
        nav: cv::nav(key, current.config.as_ref(), None),
        nav_oob: true,
    })
}

fn status_of(errors: bool) -> StatusCode {
    if errors {
        StatusCode::UNPROCESSABLE_ENTITY
    } else {
        StatusCode::OK
    }
}

fn raw_view(text: &str, form: &FormData, current: &Current) -> RawView {
    let check = cv::check_raw(text, &current.text, current.config.as_ref());
    RawView {
        text: text.to_owned(),
        problems: check.problems,
        changes: check.changes.len(),
        dirty: check.dirty,
        opened: form
            .last("opened")
            .map_or_else(|| cv::hash(&current.text), str::to_owned),
    }
}

/// Validates a section form (applying any structural button first) against the file. A
/// form refused because the file's rows moved (`Checked::moved`) comes back as the section
/// fresh from the file, its edits dropped, with the reason on top.
fn edited_section(
    web: &WebState,
    current: &Current,
    form: &mut FormData,
) -> Result<(SectionView, Checked), WebError> {
    let (key, gate) = section_key(form.last("section").unwrap_or_default())?;
    if current.config.is_none() {
        // Never edit a stand-in document: the save would replace the whole file.
        return Err(WebError::BadRequest(
            "The configuration file does not validate; fix it in the Raw TOML tab.".to_owned(),
        ));
    }
    let snapshot = web.app.snapshot();
    if let Some(op) = form.last("op").map(str::to_owned) {
        cv::apply_op(form, &op, &current.doc, &snapshot);
    }
    let checked = if gate.editable() {
        cv::validate_section(key, &current.doc, current.config.as_ref(), form)
    } else {
        Checked {
            config: current.config.clone(),
            ..Checked::default()
        }
    };
    if let Some(moved) = checked.moved {
        let s = fresh_section(web, current, key, gate, vec![moved.to_owned()]);
        return Ok((s, checked));
    }
    let s = cv::section_view(key, gate, &current.disk(), form, &checked, &snapshot);
    Ok((s, checked))
}

/// The whole section swapped in. A refused form (`moved`) answers 409 and swaps the whole
/// section even where the request asked for no swap (a plain field change).
fn swap(s: SectionView, current: &Current, moved: bool) -> Result<Response, WebError> {
    let status = if moved {
        StatusCode::CONFLICT
    } else {
        status_of(s.errors > 0)
    };
    let nav = cv::nav(s.key, current.config.as_ref(), Some((s.changed, s.errors)));
    let mut response = (
        status,
        render(&SectionSwap {
            s,
            nav,
            nav_oob: true,
        })?,
    )
        .into_response();
    if moved {
        let headers = response.headers_mut();
        headers.insert("HX-Retarget", HeaderValue::from_static("#section"));
        headers.insert("HX-Reswap", HeaderValue::from_static("outerHTML"));
    }
    Ok(response)
}

/// `POST /config/validate`: a section form (or `raw`), checked against the current file.
/// A structural button (`op`) re-renders the section; a plain change returns only the state
/// slots, so inputs keep what is being typed. A form refused because the file's rows moved
/// re-renders the section from the file either way.
pub(crate) async fn validate(
    State(web): State<WebState>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    let current = Current::read(&web).await?;
    let mut form = FormData::new(pairs);
    if let Some(text) = form.raw().map(str::to_owned) {
        let raw = raw_view(&text, &form, &current);
        let status = status_of(!raw.problems.is_empty());
        return Ok(if form.has("op") {
            (status, render(&RawSwap { raw })?).into_response()
        } else {
            (status, render(&RawState { raw })?).into_response()
        });
    }
    let full = form.has("op");
    let (s, checked) = edited_section(&web, &current, &mut form)?;
    if full || checked.moved.is_some() {
        return swap(s, &current, checked.moved.is_some());
    }
    let status = status_of(s.errors > 0);
    let nav = cv::nav(s.key, current.config.as_ref(), Some((s.changed, s.errors)));
    Ok((
        status,
        render(&SectionState {
            s,
            nav,
            nav_oob: true,
        })?,
    )
        .into_response())
}

/// The review of `proposed` against the current file.
fn review(
    current: &Current,
    proposed: String,
    carried: Vec<(String, String)>,
    section: Option<&'static str>,
    stale: Option<&'static str>,
) -> ReviewView {
    let diff = cv::diff_lines(&current.text, &proposed);
    ReviewView {
        changes: cv::changes(&current.text, &proposed),
        diff_count: diff
            .iter()
            .filter(|line| matches!(line.kind, cv::DiffKind::Added | cv::DiffKind::Removed))
            .count(),
        diff,
        base: cv::hash(&current.text),
        proposed,
        carried,
        section,
        stale,
    }
}

const RAW_MOVED: &str = "The file changed since you opened the raw editor (for example a mode switch). \
     The diff below undoes that change; go back and reload the raw editor to start from the current file.";
const FILE_MOVED: &str =
    "The file changed since you opened the editor; this is the diff against the file as it is now.";

/// The review for a form or raw text, or the editor again when it does not validate.
fn review_or_editor(
    web: &WebState,
    current: &Current,
    mut form: FormData,
    stale: Option<&'static str>,
) -> Result<Response, WebError> {
    if let Some(text) = form.raw().map(str::to_owned) {
        let raw = raw_view(&text, &form, current);
        if !raw.can_review() {
            let status = status_of(!raw.problems.is_empty());
            return Ok((status, render(&RawSwap { raw })?).into_response());
        }
        let stale = stale.or((raw.opened != cv::hash(&current.text)).then_some(RAW_MOVED));
        let review = review(current, text, form.carried(), None, stale);
        return Ok(render(&ReviewSwap { review })?.into_response());
    }
    let (s, checked) = edited_section(web, current, &mut form)?;
    let key = s.key;
    let proposed = s
        .can_review()
        .then(|| checked.proposed(&current.text).map(str::to_owned))
        .flatten();
    match proposed {
        Some(text) => {
            let review = review(current, text, form.carried(), Some(key), stale);
            Ok(render(&ReviewSwap { review })?.into_response())
        }
        None => swap(s, current, checked.moved.is_some()),
    }
}

/// `POST /config/preview`: the review step (what changes, what restarts, the diff).
pub(crate) async fn preview(
    State(web): State<WebState>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    let current = Current::read(&web).await?;
    let mut form = FormData::new(pairs);
    form.remove("op");
    review_or_editor(&web, &current, form, None)
}

/// The 409 answer to a save whose file moved: a fresh review against the file as it is now.
fn conflict(web: &WebState, current: &Current, mut form: FormData) -> Result<Response, WebError> {
    form.remove("op");
    let mut response = review_or_editor(web, current, form, Some(FILE_MOVED))?;
    *response.status_mut() = StatusCode::CONFLICT;
    Ok(response)
}

/// `POST /config/save`: writes the reviewed text when the file is still the one reviewed.
/// The runtime re-reads the file right before writing and refuses a moved one.
pub(crate) async fn save(
    State(web): State<WebState>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    let current = Current::read(&web).await?;
    let form = FormData::new(pairs);
    let proposed = form.last("_proposed").unwrap_or_default().to_owned();
    let section = form
        .last("section")
        .and_then(cv::section)
        .map(|(key, _)| key);
    let Some(expected) = form
        .last("_base")
        .filter(|base| *base == cv::hash(&current.text))
        .and_then(cv::parse_hash)
    else {
        return conflict(&web, &current, form);
    };
    let changes = cv::changes(&current.text, &proposed).len();
    let result = web.app.save_config(&proposed, expected).await;
    if let Err(ConfigError::Changed) = result {
        tracing::info!("configuration save refused: the file changed since it was read");
        return conflict(&web, &Current::read(&web).await?, form);
    }
    let (code, error) = match result {
        Ok(()) => {
            tracing::info!(changes, "configuration saved via web UI");
            (StatusCode::OK, None)
        }
        Err(error) => {
            tracing::warn!(%error, "configuration save failed");
            let code = match error {
                ConfigError::Parse | ConfigError::Validation(_) | ConfigError::MissingSecret(_) => {
                    StatusCode::UNPROCESSABLE_ENTITY
                }
                ConfigError::Changed => StatusCode::CONFLICT,
                ConfigError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
                ConfigError::Io(_) | ConfigError::Serialize(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (code, Some(error.to_string()))
        }
    };
    // The nav's badges (role count, this section's changes) follow the file as it now is.
    let file = if error.is_none() {
        &proposed
    } else {
        &current.text
    };
    let nav = section.map_or_else(Vec::new, |key| {
        cv::nav(key, Config::parse(file).ok().as_ref(), None)
    });
    let saved = SavedView {
        ok: error.is_none(),
        at: view::hms(&chrono::Utc::now()),
        changes,
        error,
        carried: form.carried(),
        section,
    };
    let status = Status::new(&web.app.snapshot(), true);
    let swap = SavedSwap {
        saved,
        status,
        nav,
        nav_oob: true,
    };
    Ok((code, render(&swap)?).into_response())
}
