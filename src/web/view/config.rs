//! The configuration editor (`/config`): the field table, document edits, validation,
//! change sentences, the review diff and the stale-file guard.
//!
//! The TOML file stays the source of truth. Every form edit is applied to the on-disk
//! document with `toml_edit`, which keeps comments, ordering and formatting; `Config` is
//! never serialised back to text. Validation parses the edited text and runs
//! `Config::validate` and `runtime::validate_reload`, the checks the save path runs.

use std::{
    borrow::Cow,
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet},
    ops::Range,
};

use toml_edit::{
    Array, ArrayOfTables, DocumentMut, InlineTable, Item, RawString, Table, TableLike, Value,
};

use super::devices::{Keys, Sentence, quirk_sentences, rule_matches};
use crate::{
    config::{Config, ConfigError, MatchConfig, PowerEvent, TvOffConfig, WakeConfig},
    runtime::validate_reload,
    state::Snapshot,
};

// ---- sections -----------------------------------------------------------------------------

/// How a top-level section is changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    /// `mode`: changed with the top-bar switch, which writes it back to the file.
    Switch,
    /// Read at start-up; `validate_reload` rejects edits.
    Restart,
    /// Editable except restart-gated subsections (`tv.wake`, `tv.off`).
    Partly,
    /// Applies on save.
    Live,
}

impl Gate {
    pub(crate) fn editable(self) -> bool {
        matches!(self, Self::Partly | Self::Live)
    }

    /// Read-only at least in part: shows the restart note and secret rows.
    pub(crate) fn restart_note(self) -> bool {
        matches!(self, Self::Restart | Self::Partly)
    }
}

/// Every top-level `Config` field, in struct order.
pub(crate) const SECTIONS: [(&str, Gate); 14] = [
    ("mode", Gate::Switch),
    ("mqtt", Gate::Restart),
    ("bridge", Gate::Live),
    ("trace", Gate::Live),
    ("home_assistant", Gate::Restart),
    ("tv", Gate::Partly),
    ("receiver", Gate::Live),
    ("presets", Gate::Live),
    ("sonos", Gate::Live),
    ("timing", Gate::Live),
    ("verify", Gate::Live),
    ("fake_tv", Gate::Live),
    ("device", Gate::Live),
    ("web", Gate::Restart),
];

/// The section key as a `'static` str, when it is one.
pub(crate) fn section(key: &str) -> Option<(&'static str, Gate)> {
    SECTIONS.into_iter().find(|(name, _)| *name == key)
}

/// The first editable section in file order.
pub(crate) fn default_section(doc: &DocumentMut) -> &'static str {
    doc.iter()
        .filter_map(|(key, _)| section(key))
        .find(|(_, gate)| gate.editable())
        .map_or("bridge", |(key, _)| key)
}

// ---- the field table ----------------------------------------------------------------------

/// Largest integer TOML can hold; the bound of fields validation leaves open.
const OPEN: u64 = i64::MAX as u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Input {
    /// Whole number in `min..=max` (the validation bounds; `max` is the type's when open).
    Int {
        min: u64,
        max: u64,
    },
    Text,
    Bool,
    Choice(&'static [&'static str]),
    /// Checkbox group over the eight power events.
    Events,
    /// Checkbox group over the configured role ids.
    Roles,
}

/// One editable key: how it is labelled, explained, bounded and described when changed.
#[derive(Debug)]
pub(crate) struct FieldDef {
    /// TOML path; array elements are `*` (`device.*.probe.port`).
    pub(crate) path: &'static str,
    pub(crate) label: &'static str,
    pub(crate) unit: &'static str,
    pub(crate) help: &'static str,
    pub(crate) input: Input,
    /// The value `Config` uses when the key is absent.
    pub(crate) default: Option<&'static str>,
    /// An empty value removes the key.
    pub(crate) optional: bool,
    /// What a change means, with `{old}` and `{new}`.
    pub(crate) sentence: Option<&'static str>,
}

const fn def(path: &'static str, label: &'static str, input: Input) -> FieldDef {
    FieldDef {
        path,
        label,
        unit: "",
        help: "",
        input,
        default: None,
        optional: false,
        sentence: None,
    }
}

impl FieldDef {
    const fn unit(mut self, unit: &'static str) -> Self {
        self.unit = unit;
        self
    }
    const fn help(mut self, help: &'static str) -> Self {
        self.help = help;
        self
    }
    const fn default(mut self, default: &'static str) -> Self {
        self.default = Some(default);
        self
    }
    const fn optional(mut self) -> Self {
        self.optional = true;
        self
    }
    const fn says(mut self, sentence: &'static str) -> Self {
        self.sentence = Some(sentence);
        self
    }

    /// The key as shown under the label: the path inside its section or row.
    pub(crate) fn key(&self) -> &'static str {
        let path = self.path;
        let rest = path.split_once('.').map_or(path, |(_, rest)| rest);
        match rest.split_once("*.") {
            Some((_, key)) => key,
            None => rest.strip_prefix("slot.*.").unwrap_or(rest),
        }
    }

    /// `Default 2 · 0–10.`, the bounds and default the form enforces.
    pub(crate) fn limits(&self) -> String {
        let range = match self.input {
            Input::Int { min, max } if max < u64::from(u32::MAX) => Some(format!("{min}–{max}")),
            Input::Int { min, .. } if min > 0 => Some(format!("at least {min}")),
            _ => None,
        };
        match (self.default, range) {
            (Some(default), Some(range)) => format!("Default {default} · {range}."),
            (Some(default), None) => format!("Default {default}."),
            (None, Some(range)) => format!("{}{}.", range[..1].to_uppercase(), &range[1..]),
            (None, None) => String::new(),
        }
    }
}

const SECONDS: &str = "seconds";
const MS: &str = "ms";
const BRIDGE_KINDS: &[&str] = &["tasmota", "aether"];
const OBSERVE_KINDS: &[&str] = &["none", "denon"];
const DEVICE_KINDS: &[&str] = &["playback", "recording"];
const TAKEOVERS: &[&str] = &["cec_standby", "cec_pause_standby", "ha_media_stop", "none"];
const PRESENCES: &[&str] = &["events", "poll"];
const PROBE_KINDS: &[&str] = &["ps5"];
const START_WAKES: &[&str] = &["off", "would", "act"];

const fn int(min: u64, max: u64) -> Input {
    Input::Int { min, max }
}

/// Every key the form edits, with the bounds `Config::validate` enforces.
pub(crate) static FIELDS: &[FieldDef] = &[
    def("bridge.kind", "Adapter", Input::Choice(BRIDGE_KINDS))
        .help("Which bridge firmware Choregos talks to."),
    def("bridge.topic", "Tasmota topic", Input::Text)
        .help("tele/<topic>/…, stat/<topic>/RESULT, cmnd/<topic>/HdmiSend."),
    def("bridge.node", "aether node", Input::Text)
        .help("aether/<node>/…; lower-case letters, digits and -, up to 16."),
    def("bridge.logical_address", "Logical address", int(0, 14))
        .help("What the bridge claims on the bus; frames to it are frames to the TV."),
    def("trace.keep_days", "Keep traces", int(0, 365))
        .unit("days")
        .default("30")
        .help("Daily replay traces kept beside the state file; 0 disables them.")
        .says("Daily traces are kept {new} days instead of {old}."),
    def("tv.entity", "TV entity", Input::Text)
        .help("The Home Assistant media player of the TV."),
    def("receiver.entity", "Receiver entity", Input::Text)
        .help("The Home Assistant media player of the receiver."),
    def("receiver.select_source", "Select source", Input::Bool)
        .help("Call media_player.select_source on takeover; the receiver may also follow Active Source by itself."),
    def("receiver.observe.kind", "Direct observation", Input::Choice(OBSERVE_KINDS))
        .default("none")
        .help("Optional device truth source for receiver power; source selection and control stay with Home Assistant."),
    def("receiver.observe.host", "Receiver host", Input::Text)
        .help("Host name or address of the receiver, no port."),
    def("receiver.observe.port", "Receiver port", int(1, 65535))
        .default("8080")
        .help("HTTP port of the receiver's status page."),
    def("presets.timezone", "Time zone", Input::Text)
        .help("IANA time zone the slots are read in, e.g. Europe/Berlin."),
    def("presets.slot.*.from", "From", Input::Text).help("HH:MM"),
    def("presets.slot.*.to", "To", Input::Text).help("HH:MM, up to 24:00"),
    def("presets.slot.*.script", "Script", Input::Text)
        .help("Home Assistant script run after a takeover in this slot."),
    def("sonos.entity", "Speakers entity", Input::Text)
        .help("The Home Assistant media player of the speakers."),
    def("sonos.mute_switch", "Mute switch", Input::Text)
        .help("Home Assistant switch toggled by the mute key."),
    def("sonos.volume_from", "Volume keys from", Input::Roles)
        .help("Roles whose remote volume keys are forwarded while the speakers play."),
    def("timing.off_delay_s", "Room off delay", int(0, OPEN))
        .unit(SECONDS)
        .help("How long to wait after the last player turns off before turning the TV, receiver and speakers off.")
        .says("The room waits {new} s instead of {old} s before turning off."),
    def("timing.takeover_settle_ms", "Takeover settle", int(0, OPEN))
        .unit(MS)
        .help("After a wake, how long to wait before switching the receiver to the new source and checking that a displaced player went to sleep.")
        .says("A takeover settles after {new} ms instead of {old} ms."),
    def("timing.standby_retries", "Standby retries", int(0, 10))
        .unit("attempts")
        .default("2")
        .help("How many times to resend Standby when a displaced player does not confirm it. Each retry is one more CEC Standby on the bus.")
        .says("A displaced player gets {new} Standby retries instead of {old}."),
    def("timing.presence_poll_s", "Presence poll", int(1, OPEN))
        .unit(SECONDS)
        .help("While a polled role is on, how often Choregos asks for its power status.")
        .says("Polled roles are asked every {new} s instead of {old} s."),
    def("timing.presence_misses", "Presence misses", int(1, u32::MAX as u64))
        .unit("polls")
        .help("Unanswered polls in a row before a polled role counts as off.")
        .says("A polled role is off after {new} unanswered polls instead of {old}."),
    def("timing.identify_retry_s", "Identify retry", int(1, OPEN))
        .unit(SECONDS)
        .help("How long to wait before asking an unidentified bus entry again.")
        .says("Unidentified entries are asked again after {new} s instead of {old} s."),
    def("verify.tv_poll_ms", "TV poll", int(500, 10_000))
        .unit(MS)
        .default("2000")
        .help("How often the TV is asked directly during an action window."),
    def("verify.tv_window_s", "TV window", int(5, 300))
        .unit(SECONDS)
        .default("45")
        .help("How long direct TV polling runs after an action; at least the TV deadline."),
    def("verify.tv_idle_poll_s", "TV idle poll", int(5, 120))
        .unit(SECONDS)
        .default("15")
        .help("Direct TV polling outside action windows, in active mode."),
    def("verify.receiver_poll_ms", "Receiver poll", int(500, 10_000))
        .unit(MS)
        .default("1000")
        .help("How often the receiver is asked directly during an action window."),
    def("verify.receiver_window_s", "Receiver window", int(5, 300))
        .unit(SECONDS)
        .default("30")
        .help("How long direct receiver polling runs after an action; at least both receiver deadlines."),
    def("verify.receiver_idle_poll_s", "Receiver idle poll", int(5, 120))
        .unit(SECONDS)
        .default("15")
        .help("Direct receiver polling outside action windows."),
    def("verify.probe_poll_ms", "Probe poll", int(500, 10_000))
        .unit(MS)
        .default("2000")
        .help("How often console probes are asked during an action window."),
    def("verify.probe_window_s", "Probe window", int(5, 300))
        .unit(SECONDS)
        .default("45")
        .help("How long probe polling runs after an action."),
    def("verify.tv_s", "TV deadline", int(1, 300))
        .unit(SECONDS)
        .default("45")
        .help("How long the TV gets to reach the intended power state before the expectation is missed.")
        .says("The TV gets {new} s instead of {old} s to reach its power state."),
    def("verify.receiver_s", "Receiver on deadline", int(1, 300))
        .unit(SECONDS)
        .default("10")
        .help("How long the receiver gets to report on after a wake.")
        .says("The receiver gets {new} s instead of {old} s to report on."),
    def("verify.receiver_off_s", "Receiver off deadline", int(1, 300))
        .unit(SECONDS)
        .default("20")
        .help("How long the receiver gets to report off; separate from the on deadline.")
        .says("The receiver gets {new} s instead of {old} s to report off."),
    def("verify.speakers_s", "Speakers deadline", int(1, 300))
        .unit(SECONDS)
        .default("5")
        .help("How long the speakers get to stop playing after a takeover.")
        .says("The speakers get {new} s instead of {old} s to stop."),
    def("verify.source_s", "Source deadline", int(1, 300))
        .unit(SECONDS)
        .default("10")
        .help("How long the receiver gets to show the new source.")
        .says("The receiver gets {new} s instead of {old} s to switch its input."),
    def("verify.drift_grace_s", "Drift grace", int(5, 600))
        .unit(SECONDS)
        .default("30")
        .help("How long the room may disagree with its intent before it is reported as drift.")
        .says("Drift is reported after {new} s instead of {old} s of disagreement."),
    def("fake_tv.replies", "Answer as the TV", Input::Bool)
        .default("true")
        .help("Answer power, version and name requests addressed to the bridge. Turn off when the bridge firmware answers them itself."),
    def("fake_tv.osd_name", "OSD name", Input::Text)
        .help("The name the fake TV reports; 1–14 ASCII characters."),
    def("device.*.role", "Role id", Input::Text)
        .help("Unique; also the Home Assistant entity suffix. a–z, 0–9 and _."),
    def("device.*.name", "Name", Input::Text).help("Display name."),
    def("device.*.source", "Receiver input", Input::Text)
        .help("The receiver source selected on takeover."),
    def("device.*.kind", "Kind", Input::Choice(DEVICE_KINDS))
        .help("CEC device kind; poll presence needs playback."),
    def("device.*.match.osd", "OSD", Input::Text)
        .optional()
        .help("OSD name, exact."),
    def("device.*.match.vendor", "Vendor", Input::Text)
        .optional()
        .help("Six hex digits, e.g. 0010FA."),
    def("device.*.match.physical", "Physical", Input::Text)
        .optional()
        .help("Dotted, e.g. 2.4.0.0."),
    def("device.*.on", "Counts as on", Input::Events)
        .help("CEC events from this device that mean it woke."),
    def("device.*.off", "Counts as off", Input::Events)
        .help("CEC events from this device that mean it went to sleep."),
    def("device.*.takeover", "When another player takes over", Input::Choice(TAKEOVERS)),
    def("device.*.verify_standby", "Verify standby after a takeover", Input::Bool)
        .default("true"),
    def("device.*.presence", "Presence", Input::Choice(PRESENCES)),
    def("device.*.media_player", "Media player", Input::Text)
        .optional()
        .help("Optional Home Assistant media player: shown for comparison, required by ha_media_stop."),
    def("device.*.probe.kind", "Probe", Input::Choice(PROBE_KINDS)),
    def("device.*.probe.host", "Host", Input::Text).help("Host name or address, no port."),
    def("device.*.probe.port", "Port", int(1, 65535)).default("9302"),
    def("device.*.probe.idle_poll_s", "Idle poll", int(1, 60))
        .unit(SECONDS)
        .default("2"),
    def("device.*.probe.start_wake", "Start wake", Input::Choice(START_WAKES)).default("off"),
    def("device.*.probe.boot_s", "Boot window", int(5, 120))
        .unit(SECONDS)
        .default("30"),
    def("device.*.probe.rest_s", "Rest", int(5, 300))
        .unit(SECONDS)
        .default("30"),
];

/// A path with array indexes or element ids (`device.2.on`, `device[kodi].on`) as the table's
/// `*` pattern (`device.*.on`).
pub(crate) fn pattern(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        out.push_str(".*");
        rest = rest[open..].split_once(']').map_or("", |(_, tail)| tail);
    }
    out.push_str(rest);
    out.split('.')
        .map(|segment| {
            if !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit()) {
                "*"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

pub(crate) fn field(path: &str) -> Option<&'static FieldDef> {
    let wanted = pattern(path);
    FIELDS.iter().find(|def| def.path == wanted)
}

fn table_field(path: &str) -> &'static FieldDef {
    field(path).unwrap_or_else(|| panic!("no field table row for {path}"))
}

/// The eight power event names, in `PowerEvent` order.
pub(crate) fn power_events() -> [&'static str; 8] {
    [
        PowerEvent::ImageViewOn,
        PowerEvent::TextViewOn,
        PowerEvent::ActiveSource,
        PowerEvent::PowerOn,
        PowerEvent::Standby,
        PowerEvent::InactiveSource,
        PowerEvent::PowerStandby,
        PowerEvent::ReportPhysicalAddress,
    ]
    .map(super::devices::power_event_word)
}

// ---- form data ----------------------------------------------------------------------------

/// The posted form, in order, and the originals it carries. Pending edits live only here:
/// every response re-renders them.
#[derive(Clone, Debug, Default)]
pub(crate) struct FormData {
    pairs: Vec<(String, String)>,
    /// What each field showed when the form first rendered it (the `_orig` blob), by form
    /// name, and the identity of every file row each row array had then (`rows:device`). A
    /// field is an edit only when its posted value differs from its original; `None` for a
    /// form posted without the blob, whose every posted field counts as an edit.
    orig: Option<BTreeMap<String, Val>>,
}

/// Names that steer the request instead of carrying a value.
const CONTROL: [&str; 5] = ["op", "section", "_proposed", "_base", INVALID];

/// The fields the page currently marks `aria-invalid`, space-separated.
const INVALID: &str = "_invalid";

/// The originals blob: a JSON object of form name → string, list of strings or null.
const ORIG: &str = "_orig";

/// The originals key holding the identities of the file rows `array` had when the form
/// opened, in file order.
fn rows_key(array: &str) -> String {
    format!("rows:{array}")
}

fn decode_orig(blob: &str) -> Option<BTreeMap<String, Val>> {
    let object: serde_json::Map<String, serde_json::Value> = serde_json::from_str(blob).ok()?;
    Some(
        object
            .into_iter()
            .filter_map(|(name, value)| {
                let value = match value {
                    serde_json::Value::Null => Val::Absent,
                    serde_json::Value::String(text) => Val::One(text),
                    serde_json::Value::Array(items) => Val::Many(
                        items
                            .into_iter()
                            .map(|item| match item {
                                serde_json::Value::String(text) => Some(text),
                                _ => None,
                            })
                            .collect::<Option<_>>()?,
                    ),
                    _ => return None,
                };
                Some((name, value))
            })
            .collect(),
    )
}

fn encode_orig(orig: &BTreeMap<String, Val>) -> String {
    let object: serde_json::Map<String, serde_json::Value> = orig
        .iter()
        .map(|(name, value)| {
            let value = match value {
                Val::Absent => serde_json::Value::Null,
                Val::One(text) => serde_json::Value::from(text.as_str()),
                Val::Many(items) => serde_json::Value::from(items.clone()),
            };
            (name.clone(), value)
        })
        .collect();
    serde_json::Value::Object(object).to_string()
}

impl FormData {
    /// The posted pairs; an `_orig` pair becomes the originals.
    pub(crate) fn new(mut pairs: Vec<(String, String)>) -> Self {
        let blob = pairs
            .iter()
            .rposition(|(key, _)| key == ORIG)
            .map(|at| pairs[at].1.clone());
        pairs.retain(|(key, _)| key != ORIG);
        Self {
            pairs,
            orig: blob.as_deref().and_then(decode_orig),
        }
    }

    pub(crate) fn last(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn has(&self, name: &str) -> bool {
        self.pairs.iter().any(|(key, _)| key == name)
    }

    /// Every nonempty value posted under `name` (checkbox groups post an empty sentinel).
    pub(crate) fn list(&self, name: &str) -> Vec<String> {
        self.pairs
            .iter()
            .filter(|(key, value)| key == name && !value.is_empty())
            .map(|(_, value)| value.clone())
            .collect()
    }

    fn set(&mut self, name: &str, value: &str) {
        self.pairs.retain(|(key, _)| key != name);
        self.pairs.push((name.to_owned(), value.to_owned()));
    }

    fn set_list(&mut self, name: &str, values: &[String]) {
        self.pairs.retain(|(key, _)| key != name);
        self.pairs.push((name.to_owned(), String::new()));
        self.pairs
            .extend(values.iter().map(|value| (name.to_owned(), value.clone())));
    }

    fn remove_prefix(&mut self, prefix: &str) {
        self.pairs.retain(|(key, _)| !key.starts_with(prefix));
    }

    pub(crate) fn remove(&mut self, name: &str) {
        self.pairs.retain(|(key, _)| key != name);
    }

    /// The value `name` showed when the form first rendered it.
    fn original(&self, name: &str) -> Option<&Val> {
        self.orig.as_ref()?.get(name)
    }

    /// Drops the originals under `prefix`: a new row reusing an index starts without any.
    fn forget(&mut self, prefix: &str) {
        if let Some(orig) = &mut self.orig {
            orig.retain(|name, _| !name.starts_with(prefix));
        }
    }

    /// The identities of the file rows of `array` the form opened with, in file order.
    fn opened_rows(&self, array: &str) -> Option<&[String]> {
        match self.original(&rows_key(array)) {
            Some(Val::Many(identities)) => Some(identities),
            _ => None,
        }
    }

    /// How many file rows of `array` the form opened with: rows past them were added to
    /// the file outside the form and are not the form's to drop.
    fn seen_rows(&self, array: &str, len: usize) -> usize {
        self.opened_rows(array).map_or(len, <[String]>::len)
    }

    /// Whether the form dropped the field `name` (a removed probe): it showed a value once
    /// and is no longer posted. A form without originals drops every field it lacks.
    fn dropped(&self, name: &str) -> bool {
        !self.has(name)
            && self.orig.as_ref().is_none_or(|orig| {
                orig.get(name)
                    .is_some_and(|original| *original != Val::Absent)
            })
    }

    /// Row indexes of `array` (`device`, `presets.slot`): every row posts its `origin`.
    pub(crate) fn rows(&self, array: &str) -> Vec<usize> {
        let mut rows: Vec<usize> = self
            .pairs
            .iter()
            .filter_map(|(key, _)| {
                key.strip_prefix(array)?
                    .strip_prefix('.')?
                    .strip_suffix(".origin")?
                    .parse()
                    .ok()
            })
            .collect();
        rows.sort_unstable();
        rows.dedup();
        rows
    }

    /// The edit fields and the originals, without the request controls: carried through
    /// review and back.
    pub(crate) fn carried(&self) -> Vec<(String, String)> {
        self.pairs
            .iter()
            .filter(|(key, _)| !CONTROL.contains(&key.as_str()))
            .cloned()
            .chain(
                self.orig
                    .as_ref()
                    .map(|orig| (ORIG.to_owned(), encode_orig(orig))),
            )
            .collect()
    }

    pub(crate) fn raw(&self) -> Option<&str> {
        self.last("raw")
    }
}

/// The form a page load starts from: one `origin` per existing row (and the probe marker),
/// every other field falls back to the file. The originals start empty: rendering the
/// section records them.
pub(crate) fn fresh_form(doc: &DocumentMut) -> FormData {
    let mut pairs = Vec::new();
    if let Some(devices) = array(doc, "device") {
        for (index, table) in devices.iter().enumerate() {
            pairs.push((format!("device.{index}.origin"), index.to_string()));
            if let Some(probe) = table.get("probe") {
                let kind = probe.get("kind").and_then(Item::as_str).unwrap_or("ps5");
                pairs.push((format!("device.{index}.probe.kind"), kind.to_owned()));
            }
        }
    }
    for index in 0..row_count(doc, "presets.slot") {
        pairs.push((format!("presets.slot.{index}.origin"), index.to_string()));
    }
    FormData {
        pairs,
        orig: Some(BTreeMap::new()),
    }
}

/// The row arrays the form edits (`[[device]]`, `[[presets.slot]]`).
const ROW_ARRAYS: [&str; 2] = ["device", "presets.slot"];

fn item_at<'a>(doc: &'a DocumentMut, path: &str) -> Option<&'a Item> {
    let mut segments = path.split('.');
    let mut item = doc.get(segments.next()?)?;
    for segment in segments {
        item = item.get(segment)?;
    }
    Some(item)
}

/// An inline array whose elements are all inline tables (`device = [{ … }]`, `device = []`)
/// as the array of tables it means.
fn inline_rows(array: &Array) -> Option<ArrayOfTables> {
    let mut tables = ArrayOfTables::new();
    for value in array {
        tables.push(value.as_inline_table()?.clone().into_table());
    }
    Some(tables)
}

/// A row array however the file writes it: `[[device]]` tables or an inline array of
/// inline tables.
fn array<'a>(doc: &'a DocumentMut, path: &str) -> Option<Cow<'a, ArrayOfTables>> {
    match item_at(doc, path)? {
        Item::ArrayOfTables(tables) => Some(Cow::Borrowed(tables)),
        Item::Value(Value::Array(array)) => inline_rows(array).map(Cow::Owned),
        _ => None,
    }
}

fn row_count(doc: &DocumentMut, path: &str) -> usize {
    array(doc, path).map_or(0, |tables| tables.len())
}

/// What identifies a file row of `array`: a role by its id; a preset slot, which has none,
/// by all of its values.
fn row_identity(array: &str, table: &Table) -> String {
    if array == "device" {
        lookup(table, "role").text()
    } else {
        let values: Vec<String> = ["from", "to", "script"]
            .iter()
            .map(|key| lookup(table, key).text())
            .collect();
        serde_json::Value::from(values).to_string()
    }
}

/// The identities of the file rows of `array`, in file order.
fn identities(doc: &DocumentMut, array: &str) -> Vec<String> {
    self::array(doc, array).map_or_else(Vec::new, |tables| {
        tables
            .iter()
            .map(|table| row_identity(array, table))
            .collect()
    })
}

const ROLES_MOVED: &str = "The roles in the file changed since you opened this form: a role was \
     removed, renamed or reordered outside it. Nothing was written. Your edits to this section \
     were dropped and the form now shows the file as it is; make them again.";
const SLOTS_MOVED: &str = "The preset slots in the file changed since you opened this form: a \
     slot was removed, changed or reordered outside it. Nothing was written. Your edits to this \
     section were dropped and the form now shows the file as it is; make them again.";

/// Why the form can no longer be applied to `doc`, when a file row it opened with is not
/// at its position any more (removed, reordered, a role renamed or a slot changed outside
/// the form): its rows edit file rows by position, so they would land on another row.
/// Rows the file gained after them are fine. A form posted without originals has no
/// identities to check.
fn rows_moved(section: &str, doc: &DocumentMut, form: &FormData) -> Option<&'static str> {
    ROW_ARRAYS
        .into_iter()
        .filter(|name| name.split('.').next() == Some(section))
        .find(|name| {
            form.opened_rows(name)
                .is_some_and(|opened| identities(doc, name).get(..opened.len()) != Some(opened))
        })
        .map(|name| {
            if name == "device" {
                ROLES_MOVED
            } else {
                SLOTS_MOVED
            }
        })
}

/// The file row a form row edits, if any.
fn origin(form: &FormData, array: &str, row: usize, len: usize) -> Option<usize> {
    form.last(&format!("{array}.{row}.origin"))?
        .parse()
        .ok()
        .filter(|origin| *origin < len)
}

/// Whether a form row edited a file row that is no longer there (the file lost rows outside
/// the form): such a row is dropped, never recreated.
fn gone(form: &FormData, array: &str, row: usize) -> bool {
    form.last(&format!("{array}.{row}.origin"))
        .is_some_and(|origin| origin.parse::<usize>().is_ok())
}

/// Form rows in the order the edited file lists them: kept rows by their file position
/// (each file row once), then new rows.
pub(crate) fn row_order(form: &FormData, array: &str, len: usize) -> Vec<usize> {
    let mut seen = HashSet::new();
    let mut kept = Vec::new();
    let mut added = Vec::new();
    for row in form.rows(array) {
        match origin(form, array, row, len) {
            Some(file) if seen.insert(file) => kept.push((file, row)),
            Some(_) => {}
            None if gone(form, array, row) => {}
            None => added.push(row),
        }
    }
    kept.sort_unstable();
    kept.into_iter().map(|(_, row)| row).chain(added).collect()
}

/// The form row at each position of the edited row array: the kept rows, then the rows the
/// file gained outside the form (`None`; the edit leaves them in place), then the new rows.
fn edited_rows(form: &FormData, array: &str, len: usize) -> Vec<Option<usize>> {
    let order = row_order(form, array, len);
    let kept = order
        .iter()
        .take_while(|row| origin(form, array, **row, len).is_some())
        .count();
    let unseen = len.saturating_sub(form.seen_rows(array, len));
    order[..kept]
        .iter()
        .copied()
        .map(Some)
        .chain(std::iter::repeat_n(None, unseen))
        .chain(order[kept..].iter().copied().map(Some))
        .collect()
}

// ---- structural operations ------------------------------------------------------------------

fn next_row(form: &FormData, array: &str) -> usize {
    form.rows(array).last().map_or(0, |last| last + 1)
}

/// Applies a structural button (`op`) to the pending form: add or remove a role, a probe or
/// a slot, fill a match rule from a bus entry, reset a field to the file's value.
pub(crate) fn apply_op(form: &mut FormData, op: &str, doc: &DocumentMut, snapshot: &Snapshot) {
    let (verb, argument) = op.split_once(':').unwrap_or((op, ""));
    match verb {
        "add_role" => {
            let row = next_row(form, "device");
            form.forget(&format!("device.{row}."));
            let p = |key: &str| format!("device.{row}.{key}");
            for (key, value) in [
                ("origin", ""),
                ("role", ""),
                ("name", ""),
                ("source", ""),
                ("kind", "playback"),
                ("match.osd", ""),
                ("match.vendor", ""),
                ("match.physical", ""),
                ("takeover", "cec_standby"),
                ("verify_standby", "true"),
                ("presence", "events"),
                ("media_player", ""),
            ] {
                form.set(&p(key), value);
            }
            let events = power_events();
            form.set_list(
                &p("on"),
                &events[..4]
                    .iter()
                    .map(|e| (*e).to_owned())
                    .collect::<Vec<_>>(),
            );
            form.set_list(
                &p("off"),
                &events[4..7]
                    .iter()
                    .map(|e| (*e).to_owned())
                    .collect::<Vec<_>>(),
            );
        }
        "remove_role" => form.remove_prefix(&format!("device.{argument}.")),
        "add_probe" => {
            for (key, value) in [
                ("kind", "ps5"),
                ("host", ""),
                ("port", "9302"),
                ("idle_poll_s", "2"),
                ("start_wake", "off"),
                ("boot_s", "30"),
                ("rest_s", "30"),
            ] {
                form.set(&format!("device.{argument}.probe.{key}"), value);
            }
        }
        "remove_probe" => form.remove_prefix(&format!("device.{argument}.probe.")),
        "fill" => {
            let Some((row, la)) = argument.split_once(':') else {
                return;
            };
            let Some(entry) = la
                .parse::<u8>()
                .ok()
                .and_then(|la| snapshot.bus.iter().find(|e| e.logical_address == la))
            else {
                return;
            };
            let keys = Keys::of(entry);
            for (key, value) in [
                ("osd", keys.osd),
                ("vendor", keys.vendor),
                ("physical", keys.physical),
            ] {
                form.set(
                    &format!("device.{row}.match.{key}"),
                    value.as_deref().unwrap_or_default(),
                );
            }
        }
        "add_slot" => {
            let row = next_row(form, "presets.slot");
            form.forget(&format!("presets.slot.{row}."));
            for key in ["origin", "from", "to", "script"] {
                form.set(&format!("presets.slot.{row}.{key}"), "");
            }
        }
        "remove_slot" => form.remove_prefix(&format!("presets.slot.{argument}.")),
        "reset" => {
            if let Some(def) = field(argument) {
                let value = effective(def, disk_at(doc, form, argument));
                match &value {
                    Val::Many(values) => form.set_list(argument, values),
                    other => form.set(argument, &other.text()),
                }
                // Back to what the file says now: untouched again.
                if let Some(original) = form.orig.as_mut().and_then(|orig| orig.get_mut(argument)) {
                    *original = value;
                }
            }
        }
        _ => {}
    }
}

// ---- values -------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum Val {
    One(String),
    Many(Vec<String>),
    Absent,
}

impl Val {
    fn text(&self) -> String {
        match self {
            Self::One(value) => value.clone(),
            Self::Many(values) => values.join(", "),
            Self::Absent => String::new(),
        }
    }

    /// The value as the "was" note shows it.
    fn shown(&self) -> String {
        match self {
            Self::One(value) if value.is_empty() => "empty".to_owned(),
            Self::One(value) => value.clone(),
            Self::Many(values) if values.is_empty() => "none".to_owned(),
            Self::Many(values) => values.join(", "),
            Self::Absent => "not set".to_owned(),
        }
    }
}

fn val_of(item: &Item) -> Val {
    match item.as_value() {
        Some(Value::String(text)) => Val::One(text.value().clone()),
        Some(Value::Integer(number)) => Val::One(number.value().to_string()),
        Some(Value::Boolean(flag)) => Val::One(flag.value().to_string()),
        Some(Value::Array(items)) => Val::Many(
            items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map_or_else(|| item.to_string().trim().to_owned(), str::to_owned)
                })
                .collect(),
        ),
        Some(other) => Val::One(other.to_string().trim().to_owned()),
        None => Val::Absent,
    }
}

/// The value at a dotted path below `table`, through tables and inline tables.
fn lookup(table: &dyn TableLike, path: &str) -> Val {
    let (head, rest) = path.split_once('.').unwrap_or((path, ""));
    match table.get(head) {
        None => Val::Absent,
        Some(item) if rest.is_empty() => val_of(item),
        Some(item) => item
            .as_table_like()
            .map_or(Val::Absent, |inner| lookup(inner, rest)),
    }
}

/// The file's value for a form field name (`device.2.match.osd` reads the row's file entry).
fn disk_at(doc: &DocumentMut, form: &FormData, name: &str) -> Val {
    for name_of_array in ROW_ARRAYS {
        let Some(rest) = name
            .strip_prefix(name_of_array)
            .and_then(|rest| rest.strip_prefix('.'))
        else {
            continue;
        };
        let Some((row, key)) = rest.split_once('.') else {
            return Val::Absent;
        };
        let Ok(row) = row.parse::<usize>() else {
            return Val::Absent;
        };
        let Some(tables) = array(doc, name_of_array) else {
            return Val::Absent;
        };
        return origin(form, name_of_array, row, tables.len())
            .and_then(|file| tables.get(file))
            .map_or(Val::Absent, |table| lookup(table, key));
    }
    lookup(doc.as_table(), name)
}

fn effective(def: &FieldDef, value: Val) -> Val {
    match (value, def.default) {
        (Val::Absent, Some(default)) => Val::One(default.to_owned()),
        (value, _) => value,
    }
}

/// A text as an HTML textarea holds it: every line break (CRLF or a lone CR) is LF.
fn textarea(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Whether two values mean the same: numbers by value, lists as sets, text trimmed and with
/// its line breaks as a textarea normalises them (a browser shows a file's lone CR as LF and
/// posts every line break as CRLF), so an untouched text field is never an edit.
fn same(def: &FieldDef, a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::Many(a), Val::Many(b)) => {
            a.iter().collect::<HashSet<_>>() == b.iter().collect::<HashSet<_>>()
        }
        (Val::One(a), Val::One(b)) if matches!(def.input, Input::Int { .. }) => {
            match (a.trim().parse::<u64>(), b.trim().parse::<u64>()) {
                (Ok(a), Ok(b)) => a == b,
                _ => a.trim() == b.trim(),
            }
        }
        (Val::One(a), Val::One(b)) => textarea(a).trim() == textarea(b).trim(),
        (a, b) => a == b,
    }
}

/// The value the form posts for `name`, as typed; `None` when the form does not carry it.
fn posted(def: &FieldDef, form: &FormData, name: &str) -> Option<Val> {
    form.has(name).then(|| match def.input {
        Input::Events | Input::Roles => Val::Many(form.list(name)),
        Input::Bool => Val::One((form.last(name) == Some("true")).to_string()),
        _ => Val::One(form.last(name).unwrap_or_default().to_owned()),
    })
}

/// A blank optional text is an absent key.
fn normal(def: &FieldDef, value: &Val) -> Val {
    match value {
        Val::One(text) if def.optional && text.trim().is_empty() => Val::Absent,
        value => value.clone(),
    }
}

/// Whether the user changed `name`: it is posted and differs from the value the form first
/// showed (so an edit back to that value is no edit). A posted field without an original (a
/// new row's, or any field of a form posted without originals) is an edit.
fn touched(def: &FieldDef, form: &FormData, name: &str) -> bool {
    match (posted(def, form, name), form.original(name)) {
        (None, _) => false,
        (Some(value), Some(original)) => !same(def, &normal(def, &value), &normal(def, original)),
        (Some(_), None) => true,
    }
}

/// The posted value of `name`, checked against its input.
fn read(def: &FieldDef, form: &FormData, name: &str) -> Result<Val, String> {
    match def.input {
        Input::Int { min, max } => {
            let raw = form.last(name).unwrap_or_default().trim();
            if raw.is_empty() {
                return Err("Enter a whole number.".to_owned());
            }
            let number = raw
                .parse::<u64>()
                .map_err(|_| "Enter a whole number.".to_owned())?;
            if number < min || number > max {
                return Err(if max < u64::from(u32::MAX) {
                    format!("Must be between {min} and {max}.")
                } else if number < min {
                    format!("Must be at least {min}.")
                } else {
                    "Too large.".to_owned()
                });
            }
            Ok(Val::One(number.to_string()))
        }
        Input::Text => {
            let text = form.last(name).unwrap_or_default().trim();
            Ok(if text.is_empty() && def.optional {
                Val::Absent
            } else {
                Val::One(textarea(text))
            })
        }
        Input::Bool => Ok(Val::One((form.last(name) == Some("true")).to_string())),
        Input::Choice(options) => {
            let choice = form.last(name).unwrap_or_default();
            if options.contains(&choice) {
                Ok(Val::One(choice.to_owned()))
            } else {
                Err(format!("Choose one of {}.", options.join(", ")))
            }
        }
        Input::Events => {
            let events = form.list(name);
            let known = power_events();
            match events.iter().find(|event| !known.contains(&event.as_str())) {
                Some(unknown) => Err(format!("Unknown event {unknown}.")),
                None => Ok(Val::Many(events)),
            }
        }
        Input::Roles => Ok(Val::Many(form.list(name))),
    }
}

// ---- document edits -----------------------------------------------------------------------

/// A field the form could not accept, by form name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FieldError {
    pub(crate) name: String,
    pub(crate) message: String,
}

/// Replaces a value, keeping its comment and spacing; inserts it when absent.
fn put(table: &mut dyn TableLike, key: &str, mut value: Value) -> bool {
    match table.get_mut(key) {
        Some(item) => {
            if let Some(old) = item.as_value() {
                *value.decor_mut() = old.decor().clone();
            }
            *item = Item::Value(value);
            false
        }
        None => {
            table.insert(key, Item::Value(value));
            true
        }
    }
}

fn to_value(def: &FieldDef, value: &str) -> Value {
    match def.input {
        Input::Int { .. } => Value::from(value.parse::<i64>().unwrap_or_default()),
        Input::Bool => Value::from(value == "true"),
        _ => Value::from(value),
    }
}

/// Writes `new` under `key` unless it means what the table already says. Returns `None` when
/// nothing was written, else whether a key was inserted or removed (an inline table then
/// needs reformatting).
fn write(table: &mut dyn TableLike, key: &str, def: &FieldDef, new: &Val) -> Option<bool> {
    let old = effective(def, lookup(table, key));
    if same(def, new, &old) {
        return None;
    }
    Some(match new {
        Val::Absent => table.remove(key).is_some(),
        Val::One(value) => put(table, key, to_value(def, value)),
        Val::Many(values) => {
            // Keep the file's order for the entries that stay; append the new ones.
            let kept: Vec<String> = match old {
                Val::Many(old) => old.into_iter().filter(|v| values.contains(v)).collect(),
                _ => Vec::new(),
            };
            let added = values.iter().filter(|v| !kept.contains(v)).cloned();
            let mut items: Array = kept.iter().cloned().chain(added).collect();
            items.fmt();
            put(table, key, Value::Array(items))
        }
    })
}

struct Editor<'a> {
    form: &'a FormData,
    errors: Vec<FieldError>,
    /// Values written so far: a row array keeps its inline form while this stays put.
    writes: usize,
}

impl Editor<'_> {
    /// The value to write for `name`, or `None`: an untouched field is never written,
    /// whatever the file says now, and an invalid one records its error.
    fn edit(&mut self, def: &FieldDef, name: &str) -> Option<Val> {
        if !touched(def, self.form, name) {
            return None;
        }
        match read(def, self.form, name) {
            Ok(value) => Some(value),
            Err(message) => {
                self.errors.push(FieldError {
                    name: name.to_owned(),
                    message,
                });
                None
            }
        }
    }

    /// Edits `keys` of `table` from the fields `{prefix}.{key}`; returns whether a key was
    /// inserted or removed.
    fn fields(&mut self, table: &mut dyn TableLike, prefix: &str, keys: &[&str]) -> bool {
        let mut structural = false;
        for key in keys {
            let name = format!("{prefix}.{key}");
            if let Some(value) = self.edit(table_field(&name), &name)
                && let Some(inserted) = write(table, key, table_field(&name), &value)
            {
                self.writes += 1;
                structural |= inserted;
            }
        }
        structural
    }
}

/// The table at a top-level key, created (after the section before it in `Config` order)
/// when absent; `trace` and `verify` may be missing from a valid file.
fn section_table<'a>(doc: &'a mut DocumentMut, key: &str) -> Option<&'a mut dyn TableLike> {
    if doc.get(key).is_none() {
        let position = SECTIONS
            .iter()
            .take_while(|(name, _)| *name != key)
            .filter_map(|(name, _)| doc.get(name).and_then(Item::as_table)?.position())
            .last();
        let mut table = Table::new();
        if let Some(position) = position {
            table.set_position(position);
        }
        table.decor_mut().set_prefix("\n");
        doc.insert(key, Item::Table(table));
    }
    doc.get_mut(key)?.as_table_like_mut()
}

/// Applies a section's form to the document: only the fields the user changed, so an edit
/// made to the file outside the form survives. Errors are input errors (not a number, out
/// of the validated bounds, unknown choice); whole-file checks run afterwards.
pub(crate) fn apply_edits(
    doc: &mut DocumentMut,
    section: &str,
    form: &FormData,
) -> Result<Vec<Change>, Vec<FieldError>> {
    let before = doc.to_string();
    let mut editor = Editor {
        form,
        errors: Vec::new(),
        writes: 0,
    };
    match section {
        "bridge" => edit_bridge(doc, &mut editor),
        "trace" | "timing" | "verify" | "fake_tv" | "tv" | "sonos" => {
            let keys: Vec<&str> = FIELDS
                .iter()
                .filter_map(|def| def.path.strip_prefix(section)?.strip_prefix('.'))
                .filter(|key| !key.contains('.'))
                .collect();
            let existed = doc.get(section).is_some();
            if let Some(table) = section_table(doc, section) {
                editor.fields(table, section, &keys);
                if !existed && table.is_empty() {
                    doc.remove(section);
                }
            }
        }
        "receiver" => edit_receiver(doc, &mut editor),
        "presets" => edit_presets(doc, &mut editor),
        "device" => edit_devices(doc, &mut editor),
        _ => {}
    }
    if editor.errors.is_empty() {
        Ok(changes(&before, &doc.to_string()))
    } else {
        Err(editor.errors)
    }
}

fn edit_bridge(doc: &mut DocumentMut, editor: &mut Editor) {
    let Some(table) = doc.get_mut("bridge").and_then(Item::as_table_like_mut) else {
        return;
    };
    let kind_def = table_field("bridge.kind");
    let edited = editor.edit(kind_def, "bridge.kind");
    let kind = edited
        .clone()
        .unwrap_or_else(|| effective(kind_def, lookup(table, "kind")));
    let (name, other) = if kind.text() == "aether" {
        ("node", "topic")
    } else {
        ("topic", "node")
    };
    if let Some(kind) = &edited {
        write(table, "kind", kind_def, kind);
        table.remove(other);
    }
    editor.fields(table, "bridge", &[name, "logical_address"]);
}

fn edit_receiver(doc: &mut DocumentMut, editor: &mut Editor) {
    let Some(table) = doc.get_mut("receiver").and_then(Item::as_table_like_mut) else {
        return;
    };
    editor.fields(table, "receiver", &["entity", "select_source"]);
    let kind_def = table_field("receiver.observe.kind");
    let edited = editor.edit(kind_def, "receiver.observe.kind");
    let kind = edited
        .clone()
        .unwrap_or_else(|| effective(kind_def, lookup(table, "observe.kind")));
    if kind.text() == "none" {
        if edited.is_some() {
            table.remove("observe");
        }
        return;
    }
    edit_record(table, "observe", false, |observe| {
        let inserted = edited
            .as_ref()
            .and_then(|kind| write(observe, "kind", kind_def, kind))
            .unwrap_or(false);
        editor.fields(observe, "receiver.observe", &["host", "port"]) || inserted
    });
}

fn edit_presets(doc: &mut DocumentMut, editor: &mut Editor) {
    let Some(table) = doc.get_mut("presets").and_then(Item::as_table_like_mut) else {
        return;
    };
    editor.fields(table, "presets", &["timezone"]);
    edit_rows(doc, editor, "presets.slot", |editor, table, row| {
        editor.fields(
            table,
            &format!("presets.slot.{row}"),
            &["from", "to", "script"],
        );
    });
}

/// The table holding the key `name` (`device` → the root, `presets.slot` → `[presets]`).
fn parent_of<'a>(
    doc: &'a mut DocumentMut,
    name: &'a str,
) -> Option<(&'a mut dyn TableLike, &'a str)> {
    match name.rsplit_once('.') {
        None => Some((doc.as_table_mut() as &mut dyn TableLike, name)),
        Some((parent, key)) => {
            let mut segments = parent.split('.');
            let mut item = doc.get_mut(segments.next()?)?;
            for segment in segments {
                item = item.get_mut(segment)?;
            }
            Some((item.as_table_like_mut()?, key))
        }
    }
}

/// Edits a row array row by row: kept rows in place, rows the form dropped removed (their
/// leading comments move to whatever follows them), new rows appended. An inline array
/// (`device = []`, `device = [{ … }]`) is left as written until a row changes, then becomes
/// `[[device]]` tables, keeping the comments around it; an emptied array stays in the file as
/// `device = []`, because the config requires the key.
fn edit_rows(
    doc: &mut DocumentMut,
    editor: &mut Editor,
    name: &str,
    mut edit: impl FnMut(&mut Editor, &mut Table, usize),
) {
    let Some((parent, key)) = parent_of(doc, name) else {
        return;
    };
    let Some((mut key_decor, item)) = parent.get_key_value_mut(key) else {
        return;
    };
    let (inline, mut tables) = match std::mem::take(item) {
        Item::ArrayOfTables(tables) => (None, tables),
        Item::Value(Value::Array(array)) => match inline_rows(&array) {
            Some(tables) => (Some(array), tables),
            None => {
                *item = Item::Value(Value::Array(array));
                return;
            }
        },
        other => {
            *item = other;
            return;
        }
    };
    let writes = editor.writes;
    let len = tables.len();
    let order = row_order(editor.form, name, len);
    // Rows the file gained outside the form are not the form's to drop.
    let seen = editor.form.seen_rows(name, len);
    let mut kept: Vec<bool> = (0..len).map(|file| file >= seen).collect();
    let mut added = Vec::new();
    for row in order {
        match origin(editor.form, name, row, len) {
            Some(file) => {
                kept[file] = true;
                if let Some(table) = tables.get_mut(file) {
                    edit(editor, table, row);
                }
            }
            None => {
                let mut table = Table::new();
                edit(editor, &mut table, row);
                added.push(table);
            }
        }
    }
    let mut orphans = Vec::new();
    for file in (0..len).rev().filter(|file| !kept[*file]) {
        if let Some(table) = tables.get(file)
            && let Some(comments) = table
                .decor()
                .prefix()
                .and_then(RawString::as_str)
                .filter(|prefix| prefix.contains('#'))
        {
            orphans.push((table.position(), comments.to_owned()));
        }
        tables.remove(file);
    }
    let changed = editor.writes != writes || kept.contains(&false) || !added.is_empty();
    tables.extend(added);
    *item = match inline {
        Some(array) if !changed => Item::Value(Value::Array(array)),
        Some(mut array) if tables.is_empty() => {
            array.clear();
            Item::Value(Value::Array(array))
        }
        Some(array) => {
            // The new `[[…]]` headers take the key's comments and the value's trailing one.
            let leaf = key_decor.leaf_decor_mut();
            let above = leaf
                .prefix()
                .and_then(RawString::as_str)
                .unwrap_or_default();
            let above = if above.contains('#') { above } else { "\n" };
            let after = array.decor().suffix().and_then(RawString::as_str);
            if let Some(first) = tables.get_mut(0) {
                first.decor_mut().set_prefix(above);
                if let Some(after) = after.filter(|after| after.contains('#')) {
                    first.decor_mut().set_suffix(after);
                }
            }
            leaf.clear();
            Item::ArrayOfTables(tables)
        }
        None if tables.is_empty() => {
            key_decor.leaf_decor_mut().clear();
            Item::Value(Value::Array(Array::new()))
        }
        None => Item::ArrayOfTables(tables),
    };
    for (position, comments) in orphans {
        reattach(doc, position, &comments);
    }
}

/// Calls `visit` on every table below `table`, nested and array tables included.
fn visit_tables(table: &mut Table, visit: &mut dyn FnMut(&mut Table)) {
    for (_, item) in table.iter_mut() {
        match item {
            Item::Table(inner) => {
                visit(inner);
                visit_tables(inner, visit);
            }
            Item::ArrayOfTables(tables) => {
                for inner in tables.iter_mut() {
                    visit(inner);
                    visit_tables(inner, visit);
                }
            }
            _ => {}
        }
    }
}

/// Puts the comment block of a removed table before whatever now follows it in the file:
/// the next table header, else the end of the document.
fn reattach(doc: &mut DocumentMut, position: Option<usize>, comments: &str) {
    let mut next: Option<usize> = None;
    if let Some(position) = position {
        visit_tables(doc.as_table_mut(), &mut |table| {
            if let Some(at) = table.position()
                && at > position
                && next.is_none_or(|next| at < next)
            {
                next = Some(at);
            }
        });
    }
    match next {
        Some(next) => visit_tables(doc.as_table_mut(), &mut |table| {
            if table.position() == Some(next) {
                let own = table.decor().prefix().and_then(RawString::as_str);
                let prefix = format!("{comments}{}", own.unwrap_or_default());
                table.decor_mut().set_prefix(prefix);
            }
        }),
        None => {
            let tail = format!("{comments}{}", doc.trailing().as_str().unwrap_or_default());
            doc.set_trailing(tail);
        }
    }
}

fn edit_devices(doc: &mut DocumentMut, editor: &mut Editor) {
    edit_rows(doc, editor, "device", |editor, table, row| {
        let prefix = format!("device.{row}");
        editor.fields(table, &prefix, &["role", "name", "kind"]);
        edit_record_fields(
            editor,
            table,
            &prefix,
            ("match", true),
            &["osd", "vendor", "physical"],
        );
        editor.fields(
            table,
            &prefix,
            &[
                "source",
                "on",
                "off",
                "takeover",
                "verify_standby",
                "media_player",
                "presence",
            ],
        );
        let kind = format!("{prefix}.probe.kind");
        if editor.form.has(&kind) {
            edit_record_fields(
                editor,
                table,
                &prefix,
                ("probe", false),
                &[
                    "kind",
                    "host",
                    "port",
                    "idle_poll_s",
                    "start_wake",
                    "boot_s",
                    "rest_s",
                ],
            );
        } else if editor.form.dropped(&kind) && table.remove("probe").is_some() {
            editor.writes += 1;
        }
    });
}

/// Edits the record `key` of `table` (`match`, `probe`, `observe`) through `TableLike`, so a
/// `[device.match]` table is edited just like `match = { … }`. An absent record is created
/// inline when the edit writes into it, or always when `required`.
/// `edit` returns whether it inserted or removed a key: only an inline record is then
/// reformatted, a table keeps its lines.
fn edit_record(
    table: &mut dyn TableLike,
    key: &str,
    required: bool,
    edit: impl FnOnce(&mut dyn TableLike) -> bool,
) {
    let created = !table.contains_key(key);
    if created {
        table.insert(key, Item::Value(Value::InlineTable(InlineTable::new())));
    }
    let Some(item) = table.get_mut(key) else {
        return;
    };
    if let Some(inline) = item.as_inline_table_mut() {
        if edit(inline) {
            inline.fmt();
        }
        if created && !required && inline.is_empty() {
            table.remove(key);
        }
    } else if let Some(record) = item.as_table_like_mut() {
        edit(record);
    }
}

/// Edits the fields `{prefix}.{key}.*` of the record `key` (`(key, required)`).
fn edit_record_fields(
    editor: &mut Editor,
    table: &mut dyn TableLike,
    prefix: &str,
    (key, required): (&str, bool),
    keys: &[&str],
) {
    let inner = format!("{prefix}.{key}");
    edit_record(table, key, required, |record| {
        editor.fields(record, &inner, keys)
    });
}

// ---- whole-file checks ----------------------------------------------------------------------

/// A whole-file problem: the config path it is about (array indexes are file positions),
/// its line when the parser knows it, and the message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Problem {
    pub(crate) path: Option<String>,
    pub(crate) line: Option<usize>,
    pub(crate) message: String,
}

impl Problem {
    fn at(path: Option<String>, message: String) -> Self {
        Self {
            path,
            line: None,
            message,
        }
    }
}

fn message(error: &ConfigError) -> String {
    match error {
        ConfigError::Validation(message) => message.replace("..=", "–"),
        other => other.to_string(),
    }
}

/// Phrases of `Config::validate` messages and the key each is about.
const MESSAGE_KEYS: &[(&str, &str)] = &[
    ("device roles", "device.*.role"),
    ("device sources", "device.*.source"),
    ("match key", "device.*.match.osd"),
    ("vendor must", "device.*.match.vendor"),
    ("physical address", "device.*.match.physical"),
    ("poll presence", "device.*.presence"),
    ("ha_media_stop takeover", "device.*.media_player"),
    ("verify_standby = false", "device.*.on"),
    ("probe idle_poll_s", "device.*.probe.idle_poll_s"),
    ("probe boot_s", "device.*.probe.boot_s"),
    ("probe rest_s", "device.*.probe.rest_s"),
    ("probe host", "device.*.probe.host"),
    ("bridge logical address", "bridge.logical_address"),
    ("bridge topic", "bridge.topic"),
    ("bridge node", "bridge.node"),
    ("presets timezone", "presets.timezone"),
    ("preset times", "presets.slot.*"),
    ("observation host", "receiver.observe.host"),
    ("observation port", "receiver.observe.port"),
    ("presence_poll_s, presence_misses", "timing.presence_poll_s"),
    ("tv_window_s must be >=", "verify.tv_window_s"),
    ("receiver_window_s must be >=", "verify.receiver_window_s"),
];

fn whole_word(text: &str, word: &str) -> bool {
    let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
    text.match_indices(word).any(|(at, _)| {
        boundary(text[..at].chars().next_back()) && boundary(text[at + word.len()..].chars().next())
    })
}

/// The key a validation message is about, as a table pattern.
fn message_key(message: &str) -> Option<&'static str> {
    if let Some((_, path)) = MESSAGE_KEYS
        .iter()
        .find(|(phrase, _)| message.contains(phrase))
    {
        return Some(path);
    }
    FIELDS
        .iter()
        .filter(|def| !def.path.contains('*'))
        .filter(|def| {
            let leaf = def.path.rsplit('.').next().unwrap_or(def.path);
            leaf.contains('_') && whole_word(message, leaf) || whole_word(message, def.path)
        })
        .max_by_key(|def| def.path.len())
        .map(|def| def.path)
}

fn index(pattern: &str, index: usize) -> String {
    pattern.replacen('*', &index.to_string(), 1)
}

/// Parses `text` and runs the save path's checks: `Config` deserialisation, `validate`
/// (devices one at a time first, so a problem is pinned to its row), then
/// `validate_reload` against the running file. Returns the parsed config when the text
/// deserialises.
pub(crate) fn check(text: &str, current: Option<&Config>) -> (Option<Config>, Vec<Problem>) {
    if let Err(error) = text.parse::<DocumentMut>() {
        return (
            None,
            vec![parse_problem(text, error.span(), error.message())],
        );
    }
    let config: Config = match toml::from_str(text) {
        Ok(config) => config,
        Err(error) => {
            return (
                None,
                vec![parse_problem(text, error.span(), error.message())],
            );
        }
    };
    let mut problems = Vec::new();
    let mut base = config.clone();
    base.device.clear();
    let base_error = base.validate().err().map(|error| message(&error));
    for (row, device) in config.device.iter().enumerate() {
        let mut one = base.clone();
        one.device = vec![device.clone()];
        if let Err(error) = one.validate() {
            let text = message(&error);
            if Some(&text) != base_error.as_ref() {
                let path = message_key(&text)
                    .filter(|path| path.starts_with("device.*"))
                    .map_or_else(|| format!("device.{row}"), |path| index(path, row));
                problems.push(Problem::at(Some(path), text));
            }
        }
    }
    if let Some(text) = base_error {
        if text.starts_with("preset times") {
            for (row, slot) in config.presets.slot.iter().enumerate() {
                let mut one = base.clone();
                one.presets.slot = vec![slot.clone()];
                if let Err(error) = one.validate()
                    && message(&error).starts_with("preset times")
                {
                    problems.push(Problem::at(
                        Some(format!("presets.slot.{row}")),
                        message(&error),
                    ));
                }
            }
        } else {
            problems.push(Problem::at(message_key(&text).map(str::to_owned), text));
        }
    }
    if problems.is_empty()
        && let Err(error) = config.validate()
    {
        let text = message(&error);
        problems.extend(duplicate_rows(&config, &text));
        if problems.is_empty() {
            problems.push(Problem::at(message_key(&text).map(str::to_owned), text));
        }
    }
    if problems.is_empty()
        && let Some(current) = current
        && let Err(error) = validate_reload(&config, current)
    {
        problems.push(Problem::at(
            None,
            format!("Restart-gated: {}.", message(&error)),
        ));
    }
    (Some(config), problems)
}

/// Rows whose role or source is empty or repeats an earlier row's.
fn duplicate_rows(config: &Config, text: &str) -> Vec<Problem> {
    let key = if text.starts_with("device roles") {
        "role"
    } else if text.starts_with("device sources") {
        "source"
    } else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    config
        .device
        .iter()
        .enumerate()
        .filter(|(_, device)| {
            let value = if key == "role" {
                &device.role
            } else {
                &device.source
            };
            value.is_empty() || !seen.insert(value.clone())
        })
        .map(|(row, _)| Problem::at(Some(format!("device.{row}.{key}")), text.to_owned()))
        .collect()
}

fn parse_problem(text: &str, span: Option<Range<usize>>, message: &str) -> Problem {
    let message = message.trim().lines().collect::<Vec<_>>().join("; ");
    Problem {
        path: None,
        line: parse_error_position(text, span).map(|(line, _)| line),
        message,
    }
}

/// 1-based line and column of a parser error span.
pub(crate) fn parse_error_position(
    text: &str,
    span: Option<Range<usize>>,
) -> Option<(usize, usize)> {
    let start = span?.start.min(text.len());
    let before = text.get(..start)?;
    let line = before.matches('\n').count() + 1;
    let column = before
        .rsplit('\n')
        .next()
        .map_or(0, |tail| tail.chars().count())
        + 1;
    Some((line, column))
}

/// The 1-based line of a config path in `text` (`timing.standby_retries`,
/// `device.2.match.vendor`): the key's line inside its table, else the table's header.
pub(crate) fn locate(text: &str, path: &str) -> Option<usize> {
    let lines: Vec<&str> = text.lines().collect();
    let header = |line: &str| {
        let line = line.trim();
        if line.starts_with('[') {
            let name = line.trim_start_matches('[');
            Some((
                line.starts_with("[["),
                name.split(']').next().unwrap_or("").trim().to_owned(),
            ))
        } else {
            None
        }
    };
    let headers: Vec<(usize, bool, String)> = lines
        .iter()
        .enumerate()
        .filter_map(|(at, line)| header(line).map(|(is_array, name)| (at, is_array, name)))
        .collect();
    let region_end = |start: usize| {
        headers
            .iter()
            .map(|(at, ..)| *at)
            .find(|at| *at > start)
            .unwrap_or(lines.len())
    };
    let key_line = |from: usize, to: usize, key: &str| {
        (from..to).find(|at| {
            lines[*at]
                .trim_start()
                .strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        })
    };
    let segments: Vec<&str> = path.split('.').collect();
    for take in (1..=segments.len()).rev() {
        let name = segments[..take].join(".");
        let rest = &segments[take..];
        let (start, key) = match rest.first() {
            Some(row) if row.bytes().all(|b| b.is_ascii_digit()) => {
                let row: usize = row.parse().ok()?;
                let found = headers
                    .iter()
                    .filter(|(_, is_array, header)| *is_array && *header == name)
                    .nth(row);
                (found.map(|(at, ..)| *at), rest.get(1))
            }
            _ => (
                headers
                    .iter()
                    .find(|(_, is_array, header)| !*is_array && *header == name)
                    .map(|(at, ..)| *at),
                rest.first(),
            ),
        };
        if let Some(start) = start {
            return Some(
                key.and_then(|key| key_line(start + 1, region_end(start), key))
                    .unwrap_or(start)
                    + 1,
            );
        }
    }
    let first_header = headers.first().map_or(lines.len(), |(at, ..)| *at);
    key_line(0, first_header, segments.first()?).map(|at| at + 1)
}

// ---- validation of a section form -----------------------------------------------------------

/// The outcome of validating a section form against the file.
#[derive(Debug, Default)]
pub(crate) struct Checked {
    /// Field errors by form name.
    pub(crate) errors: HashMap<String, String>,
    /// Problems of a whole row (`device.3`, `presets.slot.1`) by form row prefix.
    pub(crate) rows: HashMap<String, Vec<String>>,
    /// Problems no field of this section owns.
    pub(crate) problems: Vec<String>,
    /// The edited text, when the edits applied.
    pub(crate) text: Option<String>,
    /// What the edited text changes.
    pub(crate) changes: Vec<Change>,
    /// The edited text's config (the file's before any edit), for "In words".
    pub(crate) config: Option<Config>,
    /// Why the form was refused unapplied: a file row it opened with moved (`rows_moved`).
    pub(crate) moved: Option<&'static str>,
}

impl Checked {
    /// The edited text, when it differs from the file.
    pub(crate) fn proposed(&self, disk_text: &str) -> Option<&str> {
        self.text.as_deref().filter(|text| *text != disk_text)
    }
}

/// Applies the form to a copy of the file and checks the result. A form whose rows no
/// longer address the rows it opened with is refused unapplied (`moved`), so an edit never
/// lands on another row.
pub(crate) fn validate_section(
    section: &str,
    disk: &DocumentMut,
    current: Option<&Config>,
    form: &FormData,
) -> Checked {
    if let Some(moved) = rows_moved(section, disk, form) {
        return Checked {
            moved: Some(moved),
            ..Checked::default()
        };
    }
    let mut doc = disk.clone();
    let mut checked = Checked::default();
    match apply_edits(&mut doc, section, form) {
        Err(errors) => {
            checked.errors = errors.into_iter().map(|e| (e.name, e.message)).collect();
            checked.config = None;
        }
        Ok(changes) => {
            let text = doc.to_string();
            let (config, problems) = check(&text, current);
            for problem in problems {
                place(&mut checked, section, disk, form, problem);
            }
            checked.changes = changes;
            checked.text = Some(text);
            checked.config = config;
        }
    }
    checked
}

/// Pins a whole-file problem to the field (or row) of this form it is about.
fn place(
    checked: &mut Checked,
    section: &str,
    disk: &DocumentMut,
    form: &FormData,
    problem: Problem,
) {
    let Some(path) = problem.path else {
        checked.problems.push(problem.message);
        return;
    };
    let mut name = path.clone();
    for array_name in ROW_ARRAYS {
        if let Some(rest) = path
            .strip_prefix(array_name)
            .and_then(|r| r.strip_prefix('.'))
        {
            let (file_row, key) = rest.split_once('.').unwrap_or((rest, ""));
            let len = row_count(disk, array_name);
            let rows = edited_rows(form, array_name, len);
            let Some(row) = file_row
                .parse::<usize>()
                .ok()
                .and_then(|at| rows.get(at).copied().flatten())
            else {
                checked.problems.push(problem.message);
                return;
            };
            if key.is_empty() {
                checked
                    .rows
                    .entry(format!("{array_name}.{row}"))
                    .or_default()
                    .push(problem.message);
                return;
            }
            name = format!("{array_name}.{row}.{key}");
        }
    }
    let owner = name.split('.').next().unwrap_or_default();
    if owner == section && field(&name).is_some() {
        checked.errors.entry(name).or_insert(problem.message);
    } else if owner == section {
        checked.problems.push(problem.message);
    } else {
        checked
            .problems
            .push(format!("[{owner}] {}", problem.message));
    }
}

// ---- rendering a section ---------------------------------------------------------------------

/// One rendered field.
#[derive(Clone, Debug)]
pub(crate) struct FieldView {
    pub(crate) name: String,
    /// Element id stem: `f-timing-off_delay_s`; `-s` (state) and `-e` (error) slots hang off it.
    pub(crate) id: String,
    pub(crate) key: &'static str,
    pub(crate) label: &'static str,
    pub(crate) unit: &'static str,
    pub(crate) help: &'static str,
    /// `int`, `text`, `bool`, `choice` or `multi`.
    pub(crate) kind: &'static str,
    pub(crate) min: Option<u64>,
    pub(crate) max: Option<u64>,
    pub(crate) value: String,
    pub(crate) checked: bool,
    /// Options of a choice or checkbox group, with whether each is selected.
    pub(crate) options: Vec<(String, bool)>,
    pub(crate) limits: String,
    /// The file's value, when the form differs from it.
    pub(crate) was: Option<String>,
    pub(crate) error: Option<String>,
    /// The page marks the control's validity differently (`_invalid`): a state response
    /// re-renders the control so its `aria-invalid` follows the error.
    pub(crate) restate: bool,
}

struct Builder<'a> {
    disk: &'a DocumentMut,
    form: &'a FormData,
    checked: &'a Checked,
    roles: Vec<String>,
    /// The originals of the fields rendered so far, for the form's `_orig`.
    orig: RefCell<BTreeMap<String, Val>>,
}

impl Builder<'_> {
    /// Whether `name` is a field of a row the file does not have yet.
    fn new_row(&self, name: &str) -> bool {
        ROW_ARRAYS.into_iter().any(|array_name| {
            name.strip_prefix(array_name)
                .and_then(|rest| rest.strip_prefix('.'))
                .and_then(|rest| rest.split('.').next())
                .and_then(|row| row.parse::<usize>().ok())
                .is_some_and(|row| {
                    let len = row_count(self.disk, array_name);
                    origin(self.form, array_name, row, len).is_none()
                })
        })
    }

    /// What a field shows and the original the form carries for it from here on. An edit
    /// shows as typed and keeps its original; an untouched field follows the file as it is
    /// now (`disk`). A new row's fields have no original: all of a new row is written.
    fn shown(&self, def: &FieldDef, name: &str, disk: &Val, new_row: bool) -> (Val, Option<Val>) {
        let as_typed = || posted(def, self.form, name).unwrap_or_else(|| disk.clone());
        if new_row {
            (as_typed(), None)
        } else if touched(def, self.form, name) {
            let original = self.form.original(name).unwrap_or(disk);
            (as_typed(), Some(original.clone()))
        } else {
            (disk.clone(), Some(disk.clone()))
        }
    }

    /// What the field `name` shows.
    fn value(&self, name: &str) -> String {
        let def = table_field(name);
        let disk = effective(def, disk_at(self.disk, self.form, name));
        self.shown(def, name, &disk, self.new_row(name)).0.text()
    }

    /// A field: the user's edit, else the file's value.
    fn field(&self, name: &str) -> FieldView {
        let def = table_field(name);
        let disk = effective(def, disk_at(self.disk, self.form, name));
        let new_row = self.new_row(name);
        let (value, original) = self.shown(def, name, &disk, new_row);
        if let Some(original) = original {
            self.orig.borrow_mut().insert(name.to_owned(), original);
        }
        let was = (!new_row && !same(def, &normal(def, &value), &normal(def, &disk)))
            .then(|| disk.shown());
        let (kind, options): (&'static str, Vec<(String, bool)>) = match def.input {
            Input::Int { .. } => ("int", Vec::new()),
            Input::Text => ("text", Vec::new()),
            Input::Bool => ("bool", Vec::new()),
            Input::Choice(options) => (
                "choice",
                options
                    .iter()
                    .map(|option| ((*option).to_owned(), value.text() == *option))
                    .collect(),
            ),
            Input::Events | Input::Roles => {
                let selected = match &value {
                    Val::Many(values) => values.clone(),
                    _ => Vec::new(),
                };
                let all: Vec<String> = if def.input == Input::Events {
                    power_events().iter().map(|e| (*e).to_owned()).collect()
                } else {
                    let mut all = self.roles.clone();
                    all.extend(
                        selected
                            .iter()
                            .filter(|r| !all.contains(r))
                            .cloned()
                            .collect::<Vec<_>>(),
                    );
                    all
                };
                (
                    "multi",
                    all.into_iter()
                        .map(|option| {
                            let on = selected.contains(&option);
                            (option, on)
                        })
                        .collect(),
                )
            }
        };
        let (min, max) = match def.input {
            Input::Int { min, max } => (Some(min), (max < u64::from(u32::MAX)).then_some(max)),
            _ => (None, None),
        };
        FieldView {
            name: name.to_owned(),
            id: format!("f-{}", name.replace('.', "-")),
            key: def.key(),
            label: def.label,
            unit: def.unit,
            help: def.help,
            kind,
            min,
            max,
            checked: value.text() == "true",
            value: value.text(),
            options,
            limits: def.limits(),
            was,
            restate: self.checked.errors.contains_key(name)
                != self
                    .form
                    .last(INVALID)
                    .is_some_and(|names| names.split(' ').any(|marked| marked == name)),
            error: self.checked.errors.get(name).cloned(),
        }
    }
}

/// A read-only block: restart-gated settings and the environment secrets they use.
#[derive(Clone, Debug)]
pub(crate) struct ReadGroup {
    pub(crate) title: String,
    pub(crate) rows: Vec<(String, String)>,
    pub(crate) secrets: Vec<SecretRow>,
}

/// An environment secret: only its variable name and whether it is set are ever shown.
#[derive(Clone, Debug)]
pub(crate) struct SecretRow {
    pub(crate) label: &'static str,
    pub(crate) env: String,
    pub(crate) set: bool,
}

impl SecretRow {
    pub(crate) fn tone(&self) -> &'static str {
        if self.set { "ok" } else { "warn" }
    }

    pub(crate) fn word(&self) -> &'static str {
        if self.set { "set" } else { "not set" }
    }
}

/// Every secret the configuration reads, as `(where, environment variable)`.
pub(crate) fn secret_names(config: &Config) -> Vec<(&'static str, String)> {
    let mut names = vec![
        ("mqtt.password", "CHOREGOS_MQTT_PASSWORD".to_owned()),
        ("home_assistant.token", "CHOREGOS_HA_TOKEN".to_owned()),
        ("web.session_secret", "CHOREGOS_SESSION_SECRET".to_owned()),
        (
            "web.oidc.client_secret",
            "CHOREGOS_OIDC_CLIENT_SECRET".to_owned(),
        ),
        (
            "web.local_login.password",
            "CHOREGOS_LOCAL_PASSWORD".to_owned(),
        ),
    ];
    if let WakeConfig::Http { token_env, .. } = &config.tv.wake {
        names.push(("tv.wake.token", token_env.clone()));
    }
    if let TvOffConfig::Webos { key_env, .. } = &config.tv.off {
        names.push(("tv.off.key", key_env.clone()));
    }
    names
}

/// The secrets under `prefix`, with whether each variable is set in this process.
fn secret_rows(config: &Config, prefix: &str) -> Vec<SecretRow> {
    secret_names(config)
        .into_iter()
        .filter(|(at, _)| {
            at.rsplit_once('.')
                .is_some_and(|(owner, _)| owner == prefix)
        })
        .map(|(at, env)| SecretRow {
            label: at.rsplit('.').next().unwrap_or(at),
            set: std::env::var(&env).is_ok(),
            env,
        })
        .collect()
}

/// One preset slot row.
#[derive(Clone, Debug)]
pub(crate) struct SlotForm {
    pub(crate) index: usize,
    pub(crate) origin: String,
    pub(crate) fields: Vec<FieldView>,
    pub(crate) problems: Vec<String>,
}

/// A bus entry offered beside a match rule.
#[derive(Clone, Debug)]
pub(crate) struct BusCandidate {
    pub(crate) la: u8,
    pub(crate) line: String,
}

/// One `[[device]]` row of the role editor.
#[derive(Clone, Debug)]
pub(crate) struct RoleForm {
    pub(crate) index: usize,
    pub(crate) origin: String,
    pub(crate) id: String,
    pub(crate) name: String,
    /// `vendor 0010FA · cec_pause_standby`.
    pub(crate) summary: String,
    pub(crate) new: bool,
    pub(crate) open: bool,
    pub(crate) changed: usize,
    pub(crate) errors: usize,
    pub(crate) identity: Vec<FieldView>,
    pub(crate) matching: Vec<FieldView>,
    /// Whether any match key is filled.
    pub(crate) filled: bool,
    /// Bus entries matching the filled keys.
    pub(crate) on_bus: Vec<BusCandidate>,
    /// Unbound entries to fill from when nothing matches.
    pub(crate) unbound: Vec<BusCandidate>,
    pub(crate) on: FieldView,
    pub(crate) off: FieldView,
    pub(crate) takeover: FieldView,
    pub(crate) takeover_note: &'static str,
    pub(crate) presence: FieldView,
    pub(crate) verify_standby: FieldView,
    pub(crate) media_player: FieldView,
    pub(crate) probe: Vec<FieldView>,
    pub(crate) start_wake_note: &'static str,
    /// The quirk sentences of the edited values; `None` until they parse.
    pub(crate) words: Option<Vec<Sentence>>,
    pub(crate) problems: Vec<String>,
}

impl RoleForm {
    pub(crate) fn fields(&self) -> impl Iterator<Item = &FieldView> {
        self.identity
            .iter()
            .chain(&self.matching)
            .chain([
                &self.on,
                &self.off,
                &self.takeover,
                &self.presence,
                &self.verify_standby,
                &self.media_player,
            ])
            .chain(&self.probe)
    }
}

fn takeover_note(takeover: &str) -> &'static str {
    match takeover {
        "cec_standby" => "Send CEC Standby, then ask for its power status.",
        "cec_pause_standby" => {
            "Send Pause first, then CEC Standby: for players that ignore Standby while playing."
        }
        "ha_media_stop" => "Stop playback through Home Assistant; needs a media player.",
        _ => "Leave it alone.",
    }
}

fn start_wake_note(start_wake: &str) -> &'static str {
    match start_wake {
        "would" => {
            "would: when the probe sees it start before any CEC event, log what Choregos would do, but do not wake the room yet."
        }
        "act" => "act: when the probe sees it start first, wake the room early.",
        _ => "off: a start the probe sees first only orders events; CEC decides.",
    }
}

/// The selected section, ready for its template.
#[derive(Debug)]
pub(crate) struct SectionView {
    pub(crate) key: &'static str,
    pub(crate) gate: Gate,
    pub(crate) title: String,
    pub(crate) note: &'static str,
    /// The current mode, for the `mode` section.
    pub(crate) mode: String,
    pub(crate) fields: Vec<FieldView>,
    pub(crate) groups: Vec<ReadGroup>,
    pub(crate) roles: Vec<RoleForm>,
    pub(crate) slots: Vec<SlotForm>,
    pub(crate) problems: Vec<String>,
    pub(crate) changed: usize,
    pub(crate) errors: usize,
    /// Form name of the first field with an error, for the footer.
    pub(crate) first_error: Option<(String, String)>,
    /// Whether the edits change the file text (comments or formatting included).
    pub(crate) dirty: bool,
    /// The bridge kind the form selects (which name field is live).
    pub(crate) bridge_kind: String,
    pub(crate) observe: bool,
    /// The form's `_orig`: what each field showed when the form first rendered it.
    pub(crate) orig: String,
}

impl SectionView {
    /// Every field of the section, rows included: the state response updates each.
    pub(crate) fn all_fields(&self) -> Vec<&FieldView> {
        self.fields
            .iter()
            .chain(self.roles.iter().flat_map(RoleForm::fields))
            .chain(self.slots.iter().flat_map(|slot| &slot.fields))
            .collect()
    }

    pub(crate) fn can_review(&self) -> bool {
        self.errors == 0 && self.dirty
    }

    /// The fields shown with an error, as the page's `_invalid` marker.
    pub(crate) fn invalid(&self) -> String {
        let names: Vec<&str> = self
            .all_fields()
            .into_iter()
            .filter(|field| field.error.is_some())
            .map(|field| field.name.as_str())
            .collect();
        names.join(" ")
    }
}

/// The file the editor started from: its document, text and parsed config.
pub(crate) struct Disk<'a> {
    pub(crate) doc: &'a DocumentMut,
    pub(crate) text: &'a str,
    pub(crate) config: Option<&'a Config>,
}

/// Renders a section from the file and the pending form.
pub(crate) fn section_view(
    key: &'static str,
    gate: Gate,
    source: &Disk,
    form: &FormData,
    checked: &Checked,
    snapshot: &Snapshot,
) -> SectionView {
    let (disk, disk_text, current) = (source.doc, source.text, source.config);
    let roles = current.map_or_else(Vec::new, |config| {
        config
            .device
            .iter()
            .map(|device| device.role.clone())
            .collect()
    });
    let builder = Builder {
        disk,
        form,
        checked,
        roles,
        orig: RefCell::default(),
    };
    let fields_of = |names: &[&str]| {
        names
            .iter()
            .map(|name| builder.field(name))
            .collect::<Vec<_>>()
    };
    let bridge_kind = builder.value("bridge.kind");
    let observe = builder.value("receiver.observe.kind") != "none";
    let mut view = SectionView {
        key,
        gate,
        title: format!("[{key}]"),
        note: match gate {
            Gate::Switch => "changed with the top-bar switch",
            Gate::Restart => "read-only · restart to change",
            Gate::Partly | Gate::Live => "applies on save · adapters restart · bridge resync",
        },
        mode: current.map_or_else(String::new, |config| config.mode.to_string()),
        fields: Vec::new(),
        groups: Vec::new(),
        roles: Vec::new(),
        slots: Vec::new(),
        problems: checked.problems.clone(),
        changed: 0,
        errors: 0,
        first_error: None,
        dirty: checked.proposed(disk_text).is_some(),
        bridge_kind,
        observe,
        orig: String::new(),
    };
    match key {
        "bridge" => {
            view.fields = fields_of(&[
                "bridge.kind",
                "bridge.topic",
                "bridge.node",
                "bridge.logical_address",
            ])
        }
        "trace" => view.fields = fields_of(&["trace.keep_days"]),
        "tv" => view.fields = fields_of(&["tv.entity"]),
        "receiver" => {
            view.fields = fields_of(&[
                "receiver.entity",
                "receiver.select_source",
                "receiver.observe.kind",
                "receiver.observe.host",
                "receiver.observe.port",
            ]);
        }
        "presets" => {
            view.fields = fields_of(&["presets.timezone"]);
            let len = row_count(disk, "presets.slot");
            view.slots = row_order(form, "presets.slot", len)
                .into_iter()
                .map(|row| {
                    let prefix = format!("presets.slot.{row}");
                    SlotForm {
                        index: row,
                        origin: origin(form, "presets.slot", row, len)
                            .map_or_else(String::new, |o| o.to_string()),
                        fields: ["from", "to", "script"]
                            .iter()
                            .map(|k| builder.field(&format!("{prefix}.{k}")))
                            .collect(),
                        problems: checked.rows.get(&prefix).cloned().unwrap_or_default(),
                    }
                })
                .collect();
        }
        "sonos" => {
            view.fields = fields_of(&["sonos.entity", "sonos.mute_switch", "sonos.volume_from"])
        }
        "timing" | "verify" | "fake_tv" => {
            let names: Vec<&str> = FIELDS
                .iter()
                .map(|def| def.path)
                .filter(|path| path.split_once('.').is_some_and(|(s, _)| s == key))
                .collect();
            view.fields = fields_of(&names);
        }
        "device" => view.roles = role_forms(&builder, disk, form, checked, snapshot),
        _ => {}
    }
    // Originals carry over from the posted form; fields rendered now record theirs. The
    // row identities are the file's when the form first rendered, and stay that.
    let mut orig = form.orig.clone().unwrap_or_default();
    orig.extend(builder.orig.into_inner());
    for array_name in ROW_ARRAYS {
        if array_name.split('.').next() == Some(key) {
            let opened = form
                .opened_rows(array_name)
                .map_or_else(|| identities(disk, array_name), <[String]>::to_vec);
            orig.insert(rows_key(array_name), Val::Many(opened));
        }
    }
    view.orig = encode_orig(&orig);
    if let Some(config) = current {
        view.groups = read_groups(key, config);
    }
    if key == "device" {
        let count = view.roles.len();
        view.title = format!(
            "[[device]] · {count} role{}",
            if count == 1 { "" } else { "s" }
        );
    }
    let row_counts: usize = view
        .roles
        .iter()
        .map(|role| usize::from(role.new))
        .sum::<usize>()
        + view
            .slots
            .iter()
            .filter(|slot| slot.origin.is_empty())
            .count();
    // File rows the form had and no longer has; rows added outside the form are not.
    let removed = |name: &str, kept: usize| {
        let len = row_count(disk, name);
        form.seen_rows(name, len).min(len).saturating_sub(kept)
    };
    let removed_rows = match key {
        "device" => removed("device", view.roles.iter().filter(|r| !r.new).count()),
        "presets" => removed(
            "presets.slot",
            view.slots.iter().filter(|s| !s.origin.is_empty()).count(),
        ),
        _ => 0,
    };
    let fields = view.all_fields();
    let visible = |field: &&&FieldView| match field.name.as_str() {
        "bridge.topic" => view.bridge_kind != "aether",
        "bridge.node" => view.bridge_kind == "aether",
        "receiver.observe.host" | "receiver.observe.port" => view.observe,
        _ => true,
    };
    let changed = fields
        .iter()
        .filter(visible)
        .filter(|f| f.was.is_some())
        .count();
    let errors = fields.iter().filter(|f| f.error.is_some()).count();
    let first_error = fields
        .iter()
        .find(|f| f.error.is_some())
        .map(|f| (f.name.clone(), f.id.clone()));
    view.changed = changed + row_counts + removed_rows;
    view.errors = errors
        + view.problems.len()
        + view.roles.iter().map(|r| r.problems.len()).sum::<usize>()
        + view.slots.iter().map(|s| s.problems.len()).sum::<usize>();
    view.first_error = first_error;
    view
}

fn role_forms(
    builder: &Builder,
    disk: &DocumentMut,
    form: &FormData,
    checked: &Checked,
    snapshot: &Snapshot,
) -> Vec<RoleForm> {
    let len = row_count(disk, "device");
    // Where each form row sits in the edited file, for the edited config's sentences.
    let positions = edited_rows(form, "device", len);
    row_order(form, "device", len)
        .into_iter()
        .map(|row| {
            let position = positions.iter().position(|at| *at == Some(row));
            let p = |key: &str| format!("device.{row}.{key}");
            let file = origin(form, "device", row, len);
            let identity: Vec<FieldView> = ["role", "name", "source", "kind"]
                .iter()
                .map(|k| builder.field(&p(k)))
                .collect();
            let matching: Vec<FieldView> = ["match.osd", "match.vendor", "match.physical"]
                .iter()
                .map(|k| builder.field(&p(k)))
                .collect();
            let rule = MatchConfig {
                osd: Some(matching[0].value.trim().to_owned()).filter(|v| !v.is_empty()),
                vendor: Some(matching[1].value.trim().to_owned()).filter(|v| !v.is_empty()),
                physical: Some(matching[2].value.trim().to_owned()).filter(|v| !v.is_empty()),
            };
            let filled = rule.osd.is_some() || rule.vendor.is_some() || rule.physical.is_some();
            let entries = snapshot
                .bus
                .iter()
                .filter(|entry| entry.logical_address != snapshot.bridge_logical_address);
            let candidate = |entry: &crate::state::BusEntryView| BusCandidate {
                la: entry.logical_address,
                line: Keys::of(entry).line(),
            };
            let on_bus: Vec<BusCandidate> = if filled {
                entries
                    .clone()
                    .filter(|entry| rule_matches(&rule, entry))
                    .map(candidate)
                    .collect()
            } else {
                Vec::new()
            };
            let unbound = if on_bus.is_empty() {
                entries
                    .filter(|entry| entry.matched_role.is_none())
                    .filter(|entry| {
                        entry.osd.is_some()
                            || entry.vendor.is_some()
                            || entry.physical_address.is_some()
                    })
                    .map(candidate)
                    .collect()
            } else {
                Vec::new()
            };
            let probe: Vec<FieldView> = if form.has(&p("probe.kind")) {
                [
                    "probe.kind",
                    "probe.host",
                    "probe.port",
                    "probe.idle_poll_s",
                    "probe.start_wake",
                    "probe.boot_s",
                    "probe.rest_s",
                ]
                .iter()
                .map(|k| builder.field(&p(k)))
                .collect()
            } else {
                Vec::new()
            };
            let takeover = builder.field(&p("takeover"));
            let presence = builder.field(&p("presence"));
            let start_wake = probe
                .iter()
                .find(|f| f.name.ends_with("start_wake"))
                .map_or("", |f| f.value.as_str());
            let words = checked.config.as_ref().and_then(|config| {
                config
                    .device
                    .get(position?)
                    .map(|device| quirk_sentences(device, config))
            });
            let mut role = RoleForm {
                index: row,
                origin: file.map_or_else(String::new, |o| o.to_string()),
                id: identity[0].value.clone(),
                name: identity[1].value.clone(),
                summary: String::new(),
                new: file.is_none(),
                open: false,
                changed: 0,
                errors: 0,
                identity,
                filled,
                on_bus,
                unbound,
                on: builder.field(&p("on")),
                off: builder.field(&p("off")),
                takeover_note: takeover_note(&takeover.value),
                takeover,
                presence,
                verify_standby: builder.field(&p("verify_standby")),
                media_player: builder.field(&p("media_player")),
                start_wake_note: start_wake_note(start_wake),
                probe,
                matching,
                words,
                problems: checked
                    .rows
                    .get(&format!("device.{row}"))
                    .cloned()
                    .unwrap_or_default(),
            };
            let first_key = [
                ("osd", &rule.osd),
                ("vendor", &rule.vendor),
                ("physical", &rule.physical),
            ]
            .into_iter()
            .find_map(|(key, value)| {
                value.as_ref().map(|value| match key {
                    "osd" => format!("osd \"{value}\""),
                    _ => format!("{key} {value}"),
                })
            })
            .unwrap_or_else(|| "no match key".to_owned());
            let mut summary = vec![first_key, role.takeover.value.clone()];
            if role.presence.value == "poll" {
                summary.push("poll".to_owned());
            }
            if !role.probe.is_empty() {
                summary.push("probe".to_owned());
            }
            role.summary = summary.join(" · ");
            role.changed = role.fields().filter(|f| f.was.is_some()).count()
                + usize::from(role.new)
                + usize::from(file.is_some_and(|file| {
                    array(disk, "device").is_some_and(|tables| {
                        tables
                            .get(file)
                            .is_some_and(|table| table.contains_key("probe"))
                    }) != !role.probe.is_empty()
                }));
            role.errors = role.fields().filter(|f| f.error.is_some()).count() + role.problems.len();
            role.open = role.new || role.changed > 0 || role.errors > 0;
            role
        })
        .collect()
}

fn read_groups(key: &str, config: &Config) -> Vec<ReadGroup> {
    let group = |title: &str, rows: Vec<(&str, String)>, secrets: Vec<SecretRow>| ReadGroup {
        title: title.to_owned(),
        rows: rows.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        secrets,
    };
    match key {
        "mqtt" => vec![group(
            "mqtt",
            vec![
                ("host", config.mqtt.host.clone()),
                ("port", config.mqtt.port.to_string()),
                ("username", config.mqtt.username.clone()),
                ("client_id", config.mqtt.client_id.clone()),
            ],
            secret_rows(config, "mqtt"),
        )],
        "home_assistant" => vec![group(
            "home_assistant",
            vec![("url", config.home_assistant.url.clone())],
            secret_rows(config, "home_assistant"),
        )],
        "web" => vec![
            group(
                "web",
                vec![
                    ("bind", config.web.bind.to_string()),
                    ("public_url", config.web.public_url.clone()),
                ],
                secret_rows(config, "web"),
            ),
            group(
                "web.oidc",
                vec![
                    ("issuer", config.web.oidc.issuer.clone()),
                    ("client_id", config.web.oidc.client_id.clone()),
                ],
                secret_rows(config, "web.oidc"),
            ),
            group(
                "web.local_login",
                vec![("username", config.web.local_login.username.clone())],
                secret_rows(config, "web.local_login"),
            ),
        ],
        "tv" => {
            let wake = match &config.tv.wake {
                WakeConfig::Wol {
                    mac,
                    broadcast,
                    port,
                } => vec![
                    ("kind", "wol".to_owned()),
                    ("mac", mac.clone()),
                    ("broadcast", broadcast.to_string()),
                    ("port", port.to_string()),
                ],
                WakeConfig::Http { url, header, .. } => vec![
                    ("kind", "http".to_owned()),
                    ("url", url.clone()),
                    ("header", header.clone()),
                ],
            };
            let off = match &config.tv.off {
                TvOffConfig::Ha => vec![("kind", "ha".to_owned())],
                TvOffConfig::Webos { host, port, .. } => vec![
                    ("kind", "webos".to_owned()),
                    ("host", host.clone()),
                    ("port", port.to_string()),
                ],
            };
            vec![
                group("tv.wake", wake, secret_rows(config, "tv.wake")),
                group("tv.off", off, secret_rows(config, "tv.off")),
            ]
        }
        "fake_tv" => vec![group(
            "fake_tv · fixed",
            vec![
                ("power_status", config.fake_tv.power_status.clone()),
                ("cec_version", config.fake_tv.cec_version.clone()),
            ],
            Vec::new(),
        )],
        _ => Vec::new(),
    }
}

/// The section nav: every top-level section with its badge.
#[derive(Clone, Debug)]
pub(crate) struct NavItem {
    pub(crate) key: &'static str,
    pub(crate) label: &'static str,
    /// `(tone, text)`.
    pub(crate) badge: Option<(&'static str, String)>,
    pub(crate) current: bool,
    /// Asked before leaving a section with unsaved edits (they live only in the form).
    pub(crate) confirm: Option<String>,
}

pub(crate) fn nav(
    current: &str,
    config: Option<&Config>,
    state: Option<(usize, usize)>,
) -> Vec<NavItem> {
    SECTIONS
        .iter()
        .map(|&(key, gate)| {
            let mine = key == current;
            let badge = match (mine, state) {
                (true, Some((_, errors))) if errors > 0 => Some(("bad", format!("{errors} error"))),
                (true, Some((changed, _))) if changed > 0 => {
                    Some(("info", format!("{changed} changed")))
                }
                _ => match gate {
                    Gate::Restart => Some(("dim", "restart".to_owned())),
                    Gate::Partly => Some(("dim", "partly".to_owned())),
                    Gate::Switch => None,
                    Gate::Live if key == "device" => {
                        config.map(|config| ("plain", config.device.len().to_string()))
                    }
                    Gate::Live => None,
                },
            };
            NavItem {
                key,
                label: if key == "device" { "devices" } else { key },
                badge,
                current: mine,
                confirm: match state {
                    Some((changed, _)) if changed > 0 && !mine => Some(format!(
                        "Leave [{current}]? Its {changed} unsaved change{} will be discarded.",
                        if changed == 1 { "" } else { "s" }
                    )),
                    _ => None,
                },
            }
        })
        .collect()
}

// ---- changes --------------------------------------------------------------------------------

/// One semantic change between two versions of the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Change {
    /// `timing.off_delay_s`, `device[console_b].probe.start_wake`, `device[kodi]`.
    pub(crate) path: String,
    pub(crate) old: Option<String>,
    pub(crate) new: Option<String>,
    pub(crate) sentence: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Leaf {
    /// `(TOML text, plain text)`: `"off"` and `off`.
    Value(String, String),
    /// An array-of-tables element.
    Element,
}

fn element_id(table: &Table, index: usize) -> String {
    ["role", "from"]
        .iter()
        .find_map(|key| table.get(key).and_then(Item::as_str).map(str::to_owned))
        .unwrap_or_else(|| (index + 1).to_string())
}

fn flatten_into(table: &dyn TableLike, prefix: &str, out: &mut Vec<(String, Leaf)>) {
    for (key, item) in table.iter() {
        let path = if prefix.is_empty() {
            key.to_owned()
        } else {
            format!("{prefix}.{key}")
        };
        let rows = match item {
            Item::ArrayOfTables(tables) => Some(Cow::Borrowed(tables)),
            // `device = [{ … }]` and `device = []` mean the same rows as `[[device]]`.
            Item::Value(Value::Array(array)) if ROW_ARRAYS.contains(&path.as_str()) => {
                inline_rows(array).map(Cow::Owned)
            }
            _ => None,
        };
        if let Some(rows) = rows {
            for (index, inner) in rows.iter().enumerate() {
                let element = format!("{path}[{}]", element_id(inner, index));
                out.push((element.clone(), Leaf::Element));
                flatten_into(inner, &element, out);
            }
            continue;
        }
        match item {
            Item::Table(inner) => flatten_into(inner, &path, out),
            Item::Value(Value::InlineTable(inner)) => flatten_into(inner, &path, out),
            Item::Value(value) => {
                let toml = value.clone().decorated("", "").to_string();
                let plain = value.as_str().map_or_else(|| toml.clone(), str::to_owned);
                out.push((path, Leaf::Value(toml, plain)));
            }
            Item::None | Item::ArrayOfTables(_) => {}
        }
    }
}

fn flatten(text: &str) -> Option<Vec<(String, Leaf)>> {
    let doc = text.parse::<DocumentMut>().ok()?;
    let mut out = Vec::new();
    flatten_into(doc.as_table(), "", &mut out);
    Some(out)
}

fn under(path: &str, elements: &HashSet<String>) -> bool {
    elements.iter().any(|element| {
        path.strip_prefix(element.as_str())
            .is_some_and(|rest| rest.starts_with('.'))
    })
}

/// What changes between two file texts, one sentence per changed key; a whole added or
/// removed array element (a role, a slot) is one change. A key written out with its default
/// value is no change.
pub(crate) fn changes(before: &str, after: &str) -> Vec<Change> {
    let (Some(old), Some(new)) = (flatten(before), flatten(after)) else {
        return Vec::new();
    };
    let old_map: HashMap<&str, &Leaf> = old.iter().map(|(p, l)| (p.as_str(), l)).collect();
    let new_map: HashMap<&str, &Leaf> = new.iter().map(|(p, l)| (p.as_str(), l)).collect();
    let elements = |list: &[(String, Leaf)], other: &HashMap<&str, &Leaf>| -> HashSet<String> {
        list.iter()
            .filter(|(path, leaf)| *leaf == Leaf::Element && !other.contains_key(path.as_str()))
            .map(|(path, _)| path.clone())
            .collect()
    };
    let added = elements(&new, &old_map);
    let removed = elements(&old, &new_map);
    let mut out = Vec::new();
    for (path, leaf) in &new {
        if under(path, &added) {
            continue;
        }
        match leaf {
            Leaf::Element if added.contains(path) => out.push(Change {
                path: path.clone(),
                old: None,
                new: Some(path.clone()),
                sentence: format!("Adds {}.", element_words(path)),
            }),
            Leaf::Element => {}
            Leaf::Value(toml, plain) => {
                let before = match old_map.get(path.as_str()) {
                    Some(Leaf::Value(toml, plain)) => Some((toml.clone(), plain.clone())),
                    _ => None,
                };
                if let Some(change) =
                    value_change(path, before, Some((toml.clone(), plain.clone())))
                {
                    out.push(change);
                }
            }
        }
    }
    for (path, leaf) in &old {
        if under(path, &removed) || new_map.contains_key(path.as_str()) {
            continue;
        }
        match leaf {
            Leaf::Element => out.push(Change {
                path: path.clone(),
                old: Some(path.clone()),
                new: None,
                sentence: format!("Removes {}.", element_words(path)),
            }),
            Leaf::Value(toml, plain) => {
                if let Some(change) = value_change(path, Some((toml.clone(), plain.clone())), None)
                {
                    out.push(change);
                }
            }
        }
    }
    out
}

/// `device[console_c]` → `role console_c`; `presets.slot[09:00]` → `the preset slot from 09:00`.
fn element_words(path: &str) -> String {
    let (array_name, id) = path
        .rsplit_once('[')
        .map_or((path, ""), |(name, id)| (name, id.trim_end_matches(']')));
    match array_name {
        "device" => format!("role {id}"),
        "presets.slot" => format!("the preset slot from {id}"),
        other => format!("[[{other}]] {id}"),
    }
}

fn value_change(
    path: &str,
    before: Option<(String, String)>,
    after: Option<(String, String)>,
) -> Option<Change> {
    let def = field(path);
    let default = def.and_then(|def| def.default).map(|d| {
        let toml = match def.map(|def| def.input) {
            Some(Input::Choice(_) | Input::Text) => format!("\"{d}\""),
            _ => d.to_owned(),
        };
        (toml, d.to_owned())
    });
    let before = before.or_else(|| default.clone());
    let after = after.or(default);
    if before.as_ref().map(|(toml, _)| toml) == after.as_ref().map(|(toml, _)| toml) {
        return None;
    }
    let sentence = match (def.and_then(|def| def.sentence), &before, &after) {
        (Some(template), Some((_, old)), Some((_, new))) => {
            template.replace("{old}", old).replace("{new}", new)
        }
        (_, Some((old, _)), Some((new, _))) => format!("{path}: {old} → {new}"),
        (_, None, Some((new, _))) => format!("{path} = {new} (was not set)"),
        (_, Some((old, _)), None) => format!("{path} removed (was {old})"),
        (_, None, None) => return None,
    };
    Some(Change {
        path: path.to_owned(),
        old: before.map(|(toml, _)| toml),
        new: after.map(|(toml, _)| toml),
        sentence,
    })
}

// ---- diff -----------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiffKind {
    Hunk,
    Same,
    Removed,
    Added,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiffLine {
    pub(crate) kind: DiffKind,
    pub(crate) text: String,
}

impl DiffLine {
    pub(crate) fn class(&self) -> &'static str {
        match self.kind {
            DiffKind::Hunk => "hunk",
            DiffKind::Same => "same",
            DiffKind::Removed => "del",
            DiffKind::Added => "add",
        }
    }

    pub(crate) fn sign(&self) -> &'static str {
        match self.kind {
            DiffKind::Hunk => "",
            DiffKind::Same => " ",
            DiffKind::Removed => "-",
            DiffKind::Added => "+",
        }
    }
}

/// Unchanged lines shown around each change.
const CONTEXT: usize = 2;
/// Above this many LCS cells the middle is shown as a whole replacement.
const MAX_CELLS: usize = 4_000_000;

/// `(kind, line in a, line in b)`: the position of each op in both texts.
type Op = (DiffKind, usize, usize);

fn lcs_ops(a: &[&str], b: &[&str], a0: usize, b0: usize, ops: &mut Vec<Op>) {
    let (n, m) = (a.len(), b.len());
    if (n + 1).saturating_mul(m + 1) > MAX_CELLS {
        ops.extend((0..n).map(|i| (DiffKind::Removed, a0 + i, b0)));
        ops.extend((0..m).map(|j| (DiffKind::Added, a0 + n, b0 + j)));
        return;
    }
    let width = m + 1;
    let mut table = vec![0u32; (n + 1) * width];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i * width + j] = if a[i] == b[j] {
                table[(i + 1) * width + j + 1] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            ops.push((DiffKind::Same, a0 + i, b0 + j));
            i += 1;
            j += 1;
        } else if j == m || (i < n && table[(i + 1) * width + j] >= table[i * width + j + 1]) {
            ops.push((DiffKind::Removed, a0 + i, b0 + j));
            i += 1;
        } else {
            ops.push((DiffKind::Added, a0 + i, b0 + j));
            j += 1;
        }
    }
}

/// The nearest table header at or above `at` in `lines`; an array-of-tables header names
/// its element by the `role` (or `from`) line under it.
fn header_for(lines: &[&str], at: usize) -> Option<String> {
    let at = at.min(lines.len().checked_sub(1)?);
    let start = (0..=at)
        .rev()
        .find(|i| lines[*i].trim_start().starts_with('['))?;
    let header = lines[start].trim();
    let header = header.split('#').next().unwrap_or(header).trim();
    if !header.starts_with("[[") {
        return Some(header.to_owned());
    }
    let id = lines[start + 1..]
        .iter()
        .take_while(|line| !line.trim_start().starts_with('['))
        .map(|line| line.trim())
        .find(|line| {
            ["role", "from"].iter().any(|key| {
                line.strip_prefix(key)
                    .is_some_and(|rest| rest.trim_start().starts_with('='))
            })
        });
    Some(match id {
        Some(id) => format!("{header} {}", id.split(" #").next().unwrap_or(id).trim()),
        None => header.to_owned(),
    })
}

/// A unified line diff of `a` → `b` with `@@ [table] @@` hunk headers.
pub(crate) fn diff_lines(a: &str, b: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = a.lines().collect();
    let b: Vec<&str> = b.lines().collect();
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let mut ops: Vec<Op> = (0..prefix).map(|i| (DiffKind::Same, i, i)).collect();
    lcs_ops(
        &a[prefix..a.len() - suffix],
        &b[prefix..b.len() - suffix],
        prefix,
        prefix,
        &mut ops,
    );
    ops.extend((0..suffix).map(|k| (DiffKind::Same, a.len() - suffix + k, b.len() - suffix + k)));
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, (kind, ..))| *kind != DiffKind::Same)
        .map(|(at, _)| at)
        .collect();
    let mut out = Vec::new();
    let mut at = 0;
    while at < changed.len() {
        let first = at;
        let start = changed[at].saturating_sub(CONTEXT);
        let mut end = changed[at];
        while at + 1 < changed.len() && changed[at + 1] <= end + 2 * CONTEXT + 1 {
            at += 1;
            end = changed[at];
        }
        let end = (end + CONTEXT).min(ops.len() - 1);
        // Named after the table of the first changed line, as it reads in its own text.
        let header = match ops[changed[first]] {
            (DiffKind::Removed, i, _) => header_for(&a, i),
            (_, _, j) => header_for(&b, j),
        };
        out.push(DiffLine {
            kind: DiffKind::Hunk,
            text: format!("@@ {} @@", header.unwrap_or_else(|| "top".to_owned())),
        });
        for &(kind, i, j) in &ops[start..=end] {
            let text = match kind {
                DiffKind::Removed => a[i],
                _ => b[j],
            };
            out.push(DiffLine {
                kind,
                text: text.to_owned(),
            });
        }
        at += 1;
    }
    out
}

/// The stale-file guard: the runtime's 64-bit hash of the text the editor started from.
pub(crate) fn hash(text: &str) -> String {
    format!("{:016x}", crate::runtime::config_hash(text))
}

/// The hash a review carried back, as the runtime compares it; `None` when malformed.
pub(crate) fn parse_hash(hex: &str) -> Option<u64> {
    u64::from_str_radix(hex, 16).ok()
}

// ---- raw editor -----------------------------------------------------------------------------

/// The raw editor's verdict on a text.
#[derive(Debug, Default)]
pub(crate) struct RawCheck {
    /// `(line, message)`.
    pub(crate) problems: Vec<(Option<usize>, String)>,
    pub(crate) changes: Vec<Change>,
    pub(crate) dirty: bool,
}

pub(crate) fn check_raw(text: &str, disk_text: &str, current: Option<&Config>) -> RawCheck {
    let (_, problems) = check(text, current);
    RawCheck {
        problems: problems
            .into_iter()
            .map(|problem| {
                let line = problem
                    .line
                    .or_else(|| problem.path.as_deref().and_then(|path| locate(text, path)));
                (line, problem.message)
            })
            .collect(),
        changes: changes(disk_text, text),
        dirty: text != disk_text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::view::tests::snapshot;

    const EXAMPLE: &str = include_str!("../../../choregos.example.toml");

    fn parse(text: &str) -> DocumentMut {
        text.parse().expect("test text parses")
    }

    fn device_mut(doc: &mut DocumentMut, index: usize) -> &mut Table {
        doc["device"]
            .as_array_of_tables_mut()
            .and_then(|tables| tables.get_mut(index))
            .expect("the example has the device")
    }

    /// The example with a receiver observation and a probe on Console B, so every field of
    /// the table has something to edit.
    fn rich() -> String {
        let mut doc = parse(EXAMPLE);
        let mut observe = InlineTable::new();
        observe.insert("kind", "denon".into());
        observe.insert("host", "receiver.lan".into());
        observe.fmt();
        doc["receiver"]["observe"] = Item::Value(Value::InlineTable(observe));
        let mut probe = InlineTable::new();
        probe.insert("kind", "ps5".into());
        probe.insert("host", "console.lan".into());
        probe.insert("start_wake", "off".into());
        probe.fmt();
        device_mut(&mut doc, 2)["probe"] = Item::Value(Value::InlineTable(probe));
        doc.to_string()
    }

    /// What the browser posts for `key` as rendered from `text` and `form`: every field, a
    /// hidden `false` before each checkbox and a sentinel before each checkbox group.
    fn browser(key: &str, text: &str, form: &FormData) -> FormData {
        let doc = parse(text);
        let config = Config::parse(text).ok();
        let (key, gate) = section(key).expect("known section");
        let checked = Checked {
            config: config.clone(),
            ..Checked::default()
        };
        let disk = Disk {
            doc: &doc,
            text,
            config: config.as_ref(),
        };
        let view = section_view(key, gate, &disk, form, &checked, &snapshot());
        let mut pairs = vec![
            ("section".to_owned(), key.to_owned()),
            ("_orig".to_owned(), view.orig.clone()),
        ];
        for role in &view.roles {
            pairs.push((format!("device.{}.origin", role.index), role.origin.clone()));
        }
        for slot in &view.slots {
            pairs.push((
                format!("presets.slot.{}.origin", slot.index),
                slot.origin.clone(),
            ));
        }
        for field in view.all_fields() {
            match field.kind {
                "bool" => {
                    pairs.push((field.name.clone(), "false".to_owned()));
                    if field.checked {
                        pairs.push((field.name.clone(), "true".to_owned()));
                    }
                }
                "multi" => {
                    pairs.push((field.name.clone(), String::new()));
                    pairs.extend(
                        field
                            .options
                            .iter()
                            .filter(|(_, on)| *on)
                            .map(|(option, _)| (field.name.clone(), option.clone())),
                    );
                }
                _ => pairs.push((field.name.clone(), field.value.clone())),
            }
        }
        FormData::new(pairs)
    }

    /// The page-load form of `key`, posted with `edits`.
    fn posted(key: &str, text: &str, edits: &[(&str, &str)]) -> FormData {
        let mut form = browser(key, text, &fresh_form(&parse(text)));
        for (name, value) in edits {
            form.set(name, value);
        }
        form
    }

    fn edited(key: &str, text: &str, form: &FormData) -> String {
        let mut doc = parse(text);
        apply_edits(&mut doc, key, form).expect("edits apply");
        doc.to_string()
    }

    fn validated(key: &str, text: &str, form: &FormData) -> Checked {
        let config = Config::parse(text).ok();
        validate_section(key, &parse(text), config.as_ref(), form)
    }

    /// Whether `path` (`*` = any element) is a live key of `doc`.
    fn exists(item: &Item, segments: &[&str]) -> bool {
        match segments.split_first() {
            None => true,
            Some((&"*", rest)) => item
                .as_array_of_tables()
                .is_some_and(|tables| tables.iter().any(|t| exists(&Item::Table(t.clone()), rest))),
            Some((segment, rest)) => item.get(segment).is_some_and(|inner| exists(inner, rest)),
        }
    }

    #[test]
    fn every_table_key_is_in_the_example_live_or_in_its_comments() {
        let doc = Item::Table(parse(EXAMPLE).as_table().clone());
        let comments: Vec<&str> = EXAMPLE
            .lines()
            .filter_map(|line| line.split_once('#').map(|(_, comment)| comment))
            .collect();
        for def in FIELDS {
            let segments: Vec<&str> = def.path.split('.').collect();
            let leaf = segments.last().unwrap();
            let documented = comments
                .iter()
                .any(|comment| comment.contains(&format!("{leaf} =")));
            assert!(
                exists(&doc, &segments) || documented,
                "{} is neither a key of nor documented in choregos.example.toml",
                def.path
            );
        }
    }

    #[test]
    fn every_rendered_field_has_a_table_row_and_every_row_is_rendered() {
        let text = rich();
        let mut rendered = HashSet::new();
        for (key, gate) in SECTIONS.iter().filter(|(_, gate)| gate.editable()) {
            let doc = parse(&text);
            let config = Config::parse(&text).unwrap();
            let checked = Checked::default();
            let disk = Disk {
                doc: &doc,
                text: &text,
                config: Some(&config),
            };
            let view = section_view(key, *gate, &disk, &fresh_form(&doc), &checked, &snapshot());
            for f in view.all_fields() {
                assert!(field(&f.name).is_some(), "{} has no table row", f.name);
                rendered.insert(pattern(&f.name));
            }
        }
        for def in FIELDS {
            assert!(
                rendered.contains(def.path),
                "{} is never rendered",
                def.path
            );
        }
    }

    #[test]
    fn integer_bounds_are_no_looser_than_config_validate() {
        let text = rich();
        fn set_in(table: &mut dyn TableLike, path: &str, value: Item) {
            match path.split_once('.') {
                None => {
                    table.insert(path, value);
                }
                Some((head, rest)) => {
                    let inner = table
                        .get_mut(head)
                        .and_then(Item::as_table_like_mut)
                        .unwrap();
                    set_in(inner, rest, value);
                }
            }
        }
        let set = |path: &str, number: u64| {
            let mut doc = parse(&text);
            let value = toml_edit::value(i64::try_from(number).unwrap());
            match path.strip_prefix("device.*.") {
                Some(rest) => set_in(device_mut(&mut doc, 2), rest, value),
                None => set_in(doc.as_table_mut(), path, value),
            }
            doc.to_string()
        };
        let mut checked = 0;
        for def in FIELDS {
            let Input::Int { min, max } = def.input else {
                continue;
            };
            if max < u64::from(u32::MAX) {
                assert!(
                    Config::parse(&set(def.path, max + 1)).is_err(),
                    "{} above {max}",
                    def.path
                );
                checked += 1;
            }
            if min > 0 {
                assert!(
                    Config::parse(&set(def.path, min - 1)).is_err(),
                    "{} below {min}",
                    def.path
                );
                checked += 1;
            }
        }
        assert!(checked > 30, "only {checked} bounds checked");
    }

    #[test]
    fn a_number_edit_rewrites_only_its_value_and_keeps_the_comment() {
        let form = posted("timing", EXAMPLE, &[("timing.off_delay_s", "5")]);
        let after = edited("timing", EXAMPLE, &form);
        assert_eq!(
            after,
            EXAMPLE.replacen("off_delay_s = 3 ", "off_delay_s = 5 ", 1)
        );
        // An unchanged form changes nothing at all.
        assert_eq!(
            edited("timing", EXAMPLE, &posted("timing", EXAMPLE, &[])),
            EXAMPLE
        );
    }

    /// `EXAMPLE` as an edit outside the form leaves it: one more standby retry.
    fn moved() -> String {
        EXAMPLE.replacen("standby_retries = 2 ", "standby_retries = 4 ", 1)
    }

    #[test]
    fn only_the_fields_the_user_changed_are_applied_to_the_current_file() {
        // Rendered from the file as it was, applied to the file as it is now.
        let form = posted("timing", EXAMPLE, &[("timing.off_delay_s", "5")]);
        let checked = validated("timing", &moved(), &form);
        assert_eq!(
            checked.text.as_deref(),
            Some(
                moved()
                    .replacen("off_delay_s = 3 ", "off_delay_s = 5 ", 1)
                    .as_str()
            ),
            "the external standby_retries = 4 is not put back to 2"
        );
        assert_eq!(
            checked
                .changes
                .iter()
                .map(|c| c.path.as_str())
                .collect::<Vec<_>>(),
            ["timing.off_delay_s"]
        );
        // A form posted without originals still applies every posted field.
        let mut legacy = form.clone();
        legacy.orig = None;
        assert!(edited("timing", &moved(), &legacy).contains("standby_retries = 2 "));
    }

    #[test]
    fn an_edit_back_to_the_original_value_is_no_edit() {
        let moved = EXAMPLE.replacen("off_delay_s = 3 ", "off_delay_s = 8 ", 1);
        for typed in ["3", " 03 "] {
            let form = posted("timing", EXAMPLE, &[("timing.off_delay_s", typed)]);
            let checked = validated("timing", &moved, &form);
            assert_eq!(checked.text.as_deref(), Some(moved.as_str()), "{typed:?}");
            assert!(checked.changes.is_empty(), "{typed:?}");
        }
        // Text compares by its lines: CRLF from a browser is the original text, so the
        // name renamed outside the form stays renamed.
        let multiline = EXAMPLE.replacen(
            "name = \"Streaming box\"",
            "name = \"\"\"Streaming\nbox\"\"\"",
            1,
        );
        let renamed = EXAMPLE.replacen("name = \"Streaming box\"", "name = \"Box\"", 1);
        let form = posted(
            "device",
            &multiline,
            &[("device.0.name", "Streaming\r\nbox")],
        );
        assert_eq!(edited("device", &renamed, &form), renamed);
    }

    #[test]
    fn a_rerender_follows_the_file_for_untouched_fields_and_keeps_edits() {
        let form = posted("timing", EXAMPLE, &[("timing.off_delay_s", "5")]);
        let moved = moved();
        let doc = parse(&moved);
        let config = Config::parse(&moved).ok();
        let checked = validate_section("timing", &doc, config.as_ref(), &form);
        let disk = Disk {
            doc: &doc,
            text: &moved,
            config: config.as_ref(),
        };
        let (key, gate) = section("timing").unwrap();
        let view = section_view(key, gate, &disk, &form, &checked, &snapshot());
        let shown = |name: &str| {
            let field = view.fields.iter().find(|f| f.name == name).unwrap();
            (field.value.clone(), field.was.clone())
        };
        assert_eq!(shown("timing.standby_retries"), ("4".to_owned(), None));
        assert_eq!(
            shown("timing.off_delay_s"),
            ("5".to_owned(), Some("3".to_owned()))
        );
        assert_eq!(view.changed, 1);
        // The untouched field's original moves to the file's value; the edit keeps its own.
        let orig = decode_orig(&view.orig).unwrap();
        assert_eq!(orig["timing.standby_retries"], Val::One("4".to_owned()));
        assert_eq!(orig["timing.off_delay_s"], Val::One("3".to_owned()));
    }

    #[test]
    fn rows_and_probes_added_or_removed_outside_the_form_survive_its_edits() {
        let snapshot = snapshot();
        let rich = rich();
        let mut form = posted("device", &rich, &[("device.0.source", "Streamer 2")]);
        apply_op(&mut form, "remove_role:1", &parse(&rich), &snapshot);
        // Outside the form: Console B loses its probe and a fifth role is appended.
        let mut doc = parse(&rich);
        device_mut(&mut doc, 2).remove("probe");
        let mut tuner = device_mut(&mut doc, 3).clone();
        tuner["source"] = toml_edit::value("Tuner");
        tuner["role"] = toml_edit::value("tuner");
        tuner["match"] = toml_edit::value(InlineTable::from_iter([("osd", "Tuner")]));
        doc["device"].as_array_of_tables_mut().unwrap().push(tuner);
        let outside = doc.to_string();
        let checked = validated("device", &outside, &form);
        assert!(checked.problems.is_empty(), "{checked:?}");
        let config = Config::parse(checked.text.as_deref().unwrap()).unwrap();
        let roles: Vec<&str> = config.device.iter().map(|d| d.role.as_str()).collect();
        assert_eq!(roles, ["streamer", "console_b", "console_a", "tuner"]);
        assert_eq!(config.device[0].source, "Streamer 2");
        assert!(config.device[1].probe.is_none(), "the probe stays removed");
        let mut paths: Vec<&str> = checked.changes.iter().map(|c| c.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, ["device[kodi]", "device[streamer].source"]);
    }

    /// `text` with the rows headed `header` (one block each, up to `end`) in `order`: an
    /// index left out is a row removed, a different order rows reordered.
    fn rows_in_order(text: &str, header: &str, end: &str, order: &[usize]) -> String {
        let start = text.find(header).unwrap();
        let stop = start + text[start..].find(end).unwrap() + 1;
        let mut heads: Vec<usize> = text[start..stop]
            .match_indices(header)
            .map(|(at, _)| start + at)
            .collect();
        heads.push(stop);
        let blocks: Vec<&str> = heads.windows(2).map(|w| &text[w[0]..w[1]]).collect();
        let rows: String = order.iter().map(|at| blocks[*at]).collect();
        format!("{}{rows}{}", &text[..start], &text[stop..])
    }

    fn roles_in_order(text: &str, order: &[usize]) -> String {
        rows_in_order(text, "[[device]]", "\n[web]", order)
    }

    fn slots_in_order(text: &str, order: &[usize]) -> String {
        rows_in_order(text, "[[presets.slot]]", "\n[sonos]", order)
    }

    #[test]
    fn a_role_removed_reordered_or_renamed_outside_the_form_refuses_it_unapplied() {
        let rich = rich();
        // The user edits Kodi (form row 1) while the file changes underneath.
        let form = posted("device", &rich, &[("device.1.source", "User Input")]);
        for outside in [
            roles_in_order(&rich, &[1, 2, 3]),
            roles_in_order(&rich, &[1, 0, 2, 3]),
            rich.replacen("role = \"kodi\"", "role = \"mediacenter\"", 1),
            // Even a role the form left alone: a removed last row is refused too.
            roles_in_order(&rich, &[0, 1, 2]),
        ] {
            let checked = validated("device", &outside, &form);
            assert_eq!(checked.moved, Some(ROLES_MOVED), "{outside}");
            assert_eq!(checked.text, None, "nothing is proposed");
            assert!(checked.changes.is_empty());
        }
        // A role appended outside the form is no move: the edit still lands on Kodi.
        let mut doc = parse(&rich);
        let mut tuner = device_mut(&mut doc, 3).clone();
        tuner["role"] = toml_edit::value("tuner");
        tuner["source"] = toml_edit::value("Tuner");
        tuner["match"] = toml_edit::value(InlineTable::from_iter([("osd", "Tuner")]));
        doc["device"].as_array_of_tables_mut().unwrap().push(tuner);
        let appended = doc.to_string();
        let checked = validated("device", &appended, &form);
        assert_eq!(checked.moved, None);
        let config = Config::parse(checked.text.as_deref().unwrap()).unwrap();
        assert_eq!(config.device[1].role, "kodi");
        assert_eq!(config.device[1].source, "User Input");
        assert_eq!(config.device[2].source, "Console B");
        assert_eq!(config.device[4].role, "tuner");
        // The identities stay the ones the form opened with across re-renders.
        let doc = parse(&appended);
        let config = Config::parse(&appended).ok();
        let disk = Disk {
            doc: &doc,
            text: &appended,
            config: config.as_ref(),
        };
        let (key, gate) = section("device").unwrap();
        let view = section_view(key, gate, &disk, &form, &checked, &snapshot());
        let orig = decode_orig(&view.orig).unwrap();
        assert_eq!(
            orig["rows:device"],
            Val::Many(
                ["streamer", "kodi", "console_b", "console_a"]
                    .map(str::to_owned)
                    .to_vec()
            )
        );
    }

    #[test]
    fn a_preset_slot_removed_reordered_or_changed_outside_the_form_refuses_it_unapplied() {
        // The user edits the evening slot (form row 1).
        let form = posted(
            "presets",
            EXAMPLE,
            &[("presets.slot.1.script", "script.listening_late")],
        );
        for outside in [
            slots_in_order(EXAMPLE, &[1, 2]),
            slots_in_order(EXAMPLE, &[1, 0, 2]),
            EXAMPLE.replacen("script.listening_day", "script.listening_morning", 1),
        ] {
            let checked = validated("presets", &outside, &form);
            assert_eq!(checked.moved, Some(SLOTS_MOVED), "{outside}");
            assert_eq!(checked.text, None, "nothing is proposed");
        }
        // A slot appended outside the form is no move: the edit still lands on the evening.
        let appended = slots_in_order(EXAMPLE, &[0, 1, 2, 2]);
        let checked = validated("presets", &appended, &form);
        assert_eq!(checked.moved, None);
        let text = checked.text.unwrap();
        assert_eq!(
            text,
            appended.replacen("script.listening_evening", "script.listening_late", 1)
        );
    }

    #[test]
    fn a_lone_carriage_return_in_an_untouched_text_is_no_edit() {
        // The file holds `name = "A\rB"`; a textarea shows it as `A\nB` and posts `A\r\nB`.
        let text = EXAMPLE.replacen("name = \"Kodi box\"", "name = \"A\\rB\"", 1);
        for shown in ["A\nB", "A\r\nB", "A\rB"] {
            let form = posted("device", &text, &[("device.1.name", shown)]);
            assert_eq!(edited("device", &text, &form), text, "{shown:?}");
            let form = posted(
                "device",
                &text,
                &[("device.1.name", shown), ("device.1.source", "Kodi 2")],
            );
            assert_eq!(
                edited("device", &text, &form),
                text.replacen("source = \"Kodi\"", "source = \"Kodi 2\"", 1),
                "only the source changes: {shown:?}"
            );
        }
        // A real edit of that text is written.
        let form = posted("device", &text, &[("device.1.name", "A\r\nC")]);
        let after = edited("device", &text, &form);
        assert_eq!(Config::parse(&after).unwrap().device[1].name, "A\nC");
        let paths: Vec<String> = changes(&text, &after).into_iter().map(|c| c.path).collect();
        assert_eq!(paths, ["device[kodi].name"]);
    }

    #[test]
    fn an_edit_on_an_irregular_file_changes_only_the_value_bytes() {
        let start = EXAMPLE.find("[timing]\n").unwrap();
        let end = start + EXAMPLE[start..].find("\n[verify]").unwrap();
        let timing = "[timing]   # odd header comment\n\n# leading comment\n\t off_delay_s\t=   3   # last device off  \n\n\ntakeover_settle_ms=2000\n  standby_retries = 2\n# between keys\npresence_poll_s = 5 # x\npresence_misses   = 2\nidentify_retry_s = 10\n";
        let text = format!("{}{timing}{}", &EXAMPLE[..start], &EXAMPLE[end..]);
        Config::parse(&text).expect("the irregular file is valid");
        let at = text.find("=   3   #").unwrap() + "=   ".len();
        let after = edited(
            "timing",
            &text,
            &posted("timing", &text, &[("timing.off_delay_s", "5")]),
        );
        assert_eq!(after.len(), text.len(), "{after}");
        assert_eq!(&after[..at], &text[..at], "every byte before the value");
        assert_eq!(&after[at..=at], "5");
        assert_eq!(
            &after[at + 1..],
            &text[at + 1..],
            "every byte after the value"
        );
        // An edit to another section leaves this one, and everything else, byte for byte.
        let after = edited(
            "verify",
            &text,
            &posted("verify", &text, &[("verify.drift_grace_s", "45")]),
        );
        let at = text.find("drift_grace_s = 30").unwrap() + "drift_grace_s = ".len();
        assert_eq!(&after[..at], &text[..at]);
        assert_eq!(&after[at..at + 2], "45");
        assert_eq!(&after[at + 2..], &text[at + 2..]);
    }

    #[test]
    fn a_normal_table_match_rule_is_edited_in_place() {
        let line = |prefix: &str| EXAMPLE.lines().find(|l| l.starts_with(prefix)).unwrap();
        let rule = line("match = { vendor = \"0010FA\" }");
        let presence = line("presence = \"events\"");
        let text = EXAMPLE.replacen(&format!("{rule}\n"), "", 1).replacen(
            &format!("{presence}\n"),
            &format!("{presence}\n[device.match]  # a table works too\nvendor = \"0010FA\"\n"),
            1,
        );
        Config::parse(&text).expect("a [device.match] table is valid");
        let form = posted("device", &text, &[("device.0.match.vendor", "0010FB")]);
        assert_eq!(
            edited("device", &text, &form),
            text.replacen("vendor = \"0010FA\"", "vendor = \"0010FB\"", 1)
        );
        // A key the table lacks is added as a line of that table, not as an inline table.
        let form = posted("device", &text, &[("device.0.match.osd", "Streamer")]);
        let after = edited("device", &text, &form);
        assert_eq!(
            after,
            text.replacen(
                "vendor = \"0010FA\"\n",
                "vendor = \"0010FA\"\nosd = \"Streamer\"\n",
                1
            )
        );
        assert_eq!(
            Config::parse(&after).unwrap().device[0]
                .matches
                .osd
                .as_deref(),
            Some("Streamer")
        );
    }

    /// The example with every role removed through the form.
    fn without_roles(text: &str) -> String {
        let mut form = posted("device", text, &[]);
        for row in 0..4 {
            apply_op(
                &mut form,
                &format!("remove_role:{row}"),
                &parse(text),
                &snapshot(),
            );
        }
        let checked = validated("device", text, &form);
        assert!(
            checked.errors.is_empty() && checked.problems.is_empty(),
            "{checked:?}"
        );
        checked.text.expect("removing every role validates")
    }

    #[test]
    fn the_last_role_leaves_an_empty_list_and_the_first_role_makes_tables_again() {
        let empty = without_roles(EXAMPLE);
        assert!(empty.contains("\ndevice = []\n"), "{empty}");
        assert!(!empty.contains("[[device]]"));
        assert!(Config::parse(&empty).unwrap().device.is_empty());
        assert!(empty.contains("# Optional Sony console truth source"));

        let text = empty.replacen(
            "device = []\n",
            "# The players\ndevice = []  # none yet\n",
            1,
        );
        let snapshot = snapshot();
        let mut form = posted("device", &text, &[]);
        apply_op(&mut form, "add_role", &parse(&text), &snapshot);
        for (name, value) in [
            ("device.0.role", "tuner"),
            ("device.0.name", "Tuner"),
            ("device.0.source", "Tuner"),
            ("device.0.match.osd", "Tuner"),
        ] {
            form.set(name, value);
        }
        let checked = validated("device", &text, &form);
        assert!(
            checked.errors.is_empty() && checked.problems.is_empty(),
            "{checked:?}"
        );
        let after = checked.text.unwrap();
        assert!(!after.contains("device = []"), "{after}");
        assert!(
            after.contains("# The players\n[[device]]  # none yet\nrole = \"tuner\"\n"),
            "{after}"
        );
        let roles: Vec<String> = Config::parse(&after)
            .unwrap()
            .device
            .into_iter()
            .map(|d| d.role)
            .collect();
        assert_eq!(roles, ["tuner"]);
        assert_eq!(
            checked
                .changes
                .iter()
                .map(|c| c.sentence.as_str())
                .collect::<Vec<_>>(),
            ["Adds role tuner."]
        );
        // An untouched inline list stays as written.
        assert_eq!(edited("device", &text, &posted("device", &text, &[])), text);
    }

    #[test]
    fn a_removed_role_hands_its_comment_block_to_what_follows() {
        let note = "# unrelated operator documentation — 雪\n";
        let first = EXAMPLE.find("[[device]]").unwrap();
        let text = format!("{}{note}{}", &EXAMPLE[..first], &EXAMPLE[first..]);
        let second =
            first + note.len() + 1 + text[first + note.len() + 1..].find("[[device]]").unwrap();
        let mut form = posted("device", &text, &[]);
        apply_op(&mut form, "remove_role:0", &parse(&text), &snapshot());
        let after = edited("device", &text, &form);
        // Byte for byte: only the first role's header and body are gone.
        assert_eq!(
            after,
            format!("{}{}", &text[..first + note.len()], &text[second - 1..])
        );

        // The last role's block (the probe notes above it) moves before `[web]`.
        let mut form = posted("device", EXAMPLE, &[]);
        apply_op(&mut form, "remove_role:3", &parse(EXAMPLE), &snapshot());
        let after = edited("device", EXAMPLE, &form);
        let notes = EXAMPLE.find("# Optional Sony").unwrap();
        let last = EXAMPLE.rfind("[[device]]").unwrap();
        let web = EXAMPLE.find("[web]").unwrap();
        assert_eq!(
            after,
            format!(
                "{}{}\n{}",
                &EXAMPLE[..notes],
                &EXAMPLE[notes..last],
                &EXAMPLE[web..]
            )
        );
    }

    #[test]
    fn bool_string_and_string_array_edits_keep_the_rest_of_the_file() {
        let form = posted("receiver", EXAMPLE, &[("receiver.select_source", "false")]);
        assert_eq!(
            edited("receiver", EXAMPLE, &form),
            EXAMPLE.replacen("select_source = true", "select_source = false", 1)
        );
        let form = posted("tv", EXAMPLE, &[("tv.entity", "media_player.big_tv")]);
        assert_eq!(
            edited("tv", EXAMPLE, &form),
            EXAMPLE.replacen("\"media_player.tv\"", "\"media_player.big_tv\"", 1)
        );
        let mut form = posted("sonos", EXAMPLE, &[]);
        form.set_list(
            "sonos.volume_from",
            &["kodi".to_owned(), "streamer".to_owned()],
        );
        assert_eq!(
            edited("sonos", EXAMPLE, &form),
            EXAMPLE.replacen(
                "volume_from = [\"streamer\"] ",
                "volume_from = [\"streamer\", \"kodi\"] ",
                1
            ),
            "kept entries keep their order; new ones are appended"
        );
        let mut form = posted("device", EXAMPLE, &[]);
        let on: Vec<String> = ["image_view_on", "text_view_on", "active_source"]
            .map(str::to_owned)
            .to_vec();
        form.set_list("device.0.on", &on);
        assert_eq!(
            edited("device", EXAMPLE, &form),
            EXAMPLE.replacen(
                "on = [\"image_view_on\", \"text_view_on\", \"active_source\", \"power_on\"]",
                "on = [\"image_view_on\", \"text_view_on\", \"active_source\"]",
                1
            )
        );
    }

    #[test]
    fn inline_table_edits_keep_the_trailing_comment() {
        let form = posted("device", EXAMPLE, &[("device.0.match.vendor", "0010FB")]);
        assert_eq!(
            edited("device", EXAMPLE, &form),
            EXAMPLE.replacen("{ vendor = \"0010FA\" }", "{ vendor = \"0010FB\" }", 1)
        );
        let form = posted("device", EXAMPLE, &[("device.0.match.osd", "Streamer")]);
        let after = edited("device", EXAMPLE, &form);
        assert!(
            after.contains(
                "match = { vendor = \"0010FA\", osd = \"Streamer\" }     # any listed key must match"
            ),
            "{after}"
        );
    }

    #[test]
    fn absent_keys_with_defaults_are_written_only_when_they_differ() {
        let text: String = EXAMPLE
            .lines()
            .filter(|line| !line.starts_with("standby_retries"))
            .map(|line| format!("{line}\n"))
            .collect();
        let same = posted("timing", &text, &[("timing.standby_retries", "2")]);
        assert_eq!(edited("timing", &text, &same), text);
        let more = posted("timing", &text, &[("timing.standby_retries", "3")]);
        assert!(
            edited("timing", &text, &more).contains("identify_retry_s = 10\nstandby_retries = 3\n")
        );
    }

    #[test]
    fn a_missing_section_is_created_after_its_predecessor_only_when_needed() {
        let text: String = EXAMPLE
            .lines()
            .filter(|line| !line.starts_with("[trace]") && !line.starts_with("keep_days"))
            .map(|line| format!("{line}\n"))
            .collect();
        assert_eq!(edited("trace", &text, &posted("trace", &text, &[])), text);
        let after = edited(
            "trace",
            &text,
            &posted("trace", &text, &[("trace.keep_days", "7")]),
        );
        let at = |needle: &str| {
            after
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} in {after}"))
        };
        assert!(at("[bridge]") < at("[trace]\nkeep_days = 7\n"));
        assert!(at("[trace]") < at("[home_assistant]"));
        assert!(Config::parse(&after).is_ok());
    }

    #[test]
    fn adding_and_removing_roles_keeps_the_other_roles_and_their_comments() {
        let snapshot = snapshot();
        let mut form = posted("device", EXAMPLE, &[]);
        apply_op(&mut form, "add_role", &parse(EXAMPLE), &snapshot);
        for (name, value) in [
            ("device.4.role", "tuner"),
            ("device.4.name", "Tuner"),
            ("device.4.source", "Tuner"),
            ("device.4.match.osd", "Tuner"),
        ] {
            form.set(name, value);
        }
        let checked = validated("device", EXAMPLE, &form);
        assert!(
            checked.errors.is_empty() && checked.problems.is_empty(),
            "{checked:?}"
        );
        let text = checked.text.unwrap();
        let config = Config::parse(&text).unwrap();
        assert_eq!(config.device.last().unwrap().role, "tuner");
        assert!(text.find("role = \"tuner\"").unwrap() < text.find("[web]").unwrap());
        assert!(
            checked
                .changes
                .iter()
                .any(|c| c.sentence == "Adds role tuner.")
        );

        let mut form = posted("device", EXAMPLE, &[]);
        apply_op(&mut form, "remove_role:1", &parse(EXAMPLE), &snapshot);
        let checked = validated("device", EXAMPLE, &form);
        let text = checked.text.unwrap();
        let roles: Vec<String> = Config::parse(&text)
            .unwrap()
            .device
            .into_iter()
            .map(|d| d.role)
            .collect();
        assert_eq!(roles, ["streamer", "console_b", "console_a"]);
        assert!(text.contains("# Optional Sony console truth source"));
        assert_eq!(
            checked
                .changes
                .iter()
                .map(|c| c.sentence.as_str())
                .collect::<Vec<_>>(),
            ["Removes role kodi."]
        );
    }

    #[test]
    fn a_new_role_needs_its_keys_and_reports_each_on_its_field() {
        let snapshot = snapshot();
        let mut form = posted("device", EXAMPLE, &[]);
        apply_op(&mut form, "add_role", &parse(EXAMPLE), &snapshot);
        let checked = validated("device", EXAMPLE, &form);
        assert!(checked.errors.contains_key("device.4.role"), "{checked:?}");
        form.set("device.4.role", "kodi");
        form.set("device.4.source", "Tuner");
        form.set("device.4.match.osd", "Tuner");
        let checked = validated("device", EXAMPLE, &form);
        assert_eq!(
            checked.errors.get("device.4.role").map(String::as_str),
            Some("device roles must be nonempty and unique"),
            "{checked:?}"
        );
    }

    #[test]
    fn adding_and_removing_a_probe_writes_only_what_differs_from_defaults() {
        let snapshot = snapshot();
        let mut form = posted("device", EXAMPLE, &[]);
        apply_op(&mut form, "add_probe:2", &parse(EXAMPLE), &snapshot);
        form.set("device.2.probe.host", "console.lan");
        let text = edited("device", EXAMPLE, &form);
        assert!(
            text.contains("probe = { kind = \"ps5\", host = \"console.lan\" }\n"),
            "{text}"
        );
        assert!(Config::parse(&text).unwrap().device[2].probe.is_some());

        let rich = rich();
        let mut form = posted("device", &rich, &[]);
        apply_op(&mut form, "remove_probe:2", &parse(&rich), &snapshot);
        let text = edited("device", &rich, &form);
        assert!(Config::parse(&text).unwrap().device[2].probe.is_none());
        assert!(text.contains("# probe = { kind = \"ps5\""), "comments stay");
    }

    #[test]
    fn fill_copies_the_bus_entry_keys_and_reset_restores_the_file_value() {
        let mut snapshot = snapshot();
        snapshot.bus.push(crate::state::BusEntryView {
            logical_address: 8,
            physical_address: Some("2.4.0.0".to_owned()),
            device_type: Some(4),
            vendor: None,
            osd: Some("Console B".to_owned()),
            power: crate::state::BusPower::On,
            last_seen: None,
            identified: true,
            verified: true,
            matched_role: None,
            confirmed_at: None,
            next_identify_at: None,
        });
        let doc = parse(EXAMPLE);
        let mut form = posted("device", EXAMPLE, &[]);
        apply_op(&mut form, "fill:2:8", &doc, &snapshot);
        assert_eq!(form.last("device.2.match.osd"), Some("Console B"));
        assert_eq!(form.last("device.2.match.vendor"), Some(""));
        assert_eq!(form.last("device.2.match.physical"), Some("2.4.0.0"));
        let text = edited("device", EXAMPLE, &form);
        assert!(
            text.contains("match = { osd = \"Console B\", physical = \"2.4.0.0\" }"),
            "{text}"
        );

        apply_op(&mut form, "reset:device.2.match.vendor", &doc, &snapshot);
        assert_eq!(form.last("device.2.match.vendor"), Some("080046"));
    }

    #[test]
    fn input_errors_stop_the_edit_and_name_their_field() {
        for (value, message) in [
            ("12", "Must be between 0 and 10."),
            ("abc", "Enter a whole number."),
            ("", "Enter a whole number."),
        ] {
            let form = posted(
                "timing",
                EXAMPLE,
                &[
                    ("timing.standby_retries", value),
                    ("timing.off_delay_s", "5"),
                ],
            );
            let errors = apply_edits(&mut parse(EXAMPLE), "timing", &form).unwrap_err();
            assert_eq!(
                errors,
                [FieldError {
                    name: "timing.standby_retries".to_owned(),
                    message: message.to_owned()
                }]
            );
        }
    }

    #[test]
    fn validation_problems_land_on_the_field_or_row_they_are_about() {
        let checked = validated(
            "device",
            EXAMPLE,
            &posted("device", EXAMPLE, &[("device.1.match.vendor", "XYZ")]),
        );
        assert_eq!(
            checked
                .errors
                .get("device.1.match.vendor")
                .map(String::as_str),
            Some("vendor must be exactly six hexadecimal digits")
        );
        let checked = validated(
            "verify",
            EXAMPLE,
            &posted("verify", EXAMPLE, &[("verify.tv_window_s", "30")]),
        );
        assert_eq!(
            checked.errors.get("verify.tv_window_s").map(String::as_str),
            Some("verify tv_window_s must be >= tv_s")
        );
        let checked = validated(
            "presets",
            EXAMPLE,
            &posted("presets", EXAMPLE, &[("presets.slot.1.from", "9pm")]),
        );
        assert_eq!(
            checked.rows.get("presets.slot.1").map(Vec::as_slice),
            Some(&["preset times must use HH:MM".to_owned()][..]),
            "{checked:?}"
        );
        let checked = validated(
            "presets",
            EXAMPLE,
            &posted("presets", EXAMPLE, &[("presets.slot.0.to", "20:00")]),
        );
        assert_eq!(
            checked.problems,
            ["preset slots must cover 24 hours without gaps or overlap"]
        );
    }

    #[test]
    fn restart_gated_edits_are_rejected_by_validate_reload() {
        let current = Config::parse(EXAMPLE).unwrap();
        let text = EXAMPLE.replacen("host = \"mqtt.lan\"", "host = \"broker.lan\"", 1);
        let (_, problems) = check(&text, Some(&current));
        assert_eq!(problems.len(), 1);
        assert!(
            problems[0]
                .message
                .contains("mqtt changes require a restart"),
            "{problems:?}"
        );
        let raw = check_raw(&text, EXAMPLE, Some(&current));
        assert_eq!(raw.problems.len(), 1);
        assert_eq!(
            raw.changes
                .iter()
                .map(|c| c.sentence.as_str())
                .collect::<Vec<_>>(),
            ["mqtt.host: \"mqtt.lan\" → \"broker.lan\""]
        );
    }

    #[test]
    fn raw_problems_carry_their_line() {
        let current = Config::parse(EXAMPLE).unwrap();
        let broken = EXAMPLE.replacen("[timing]", "[timing", 1);
        let line = EXAMPLE.lines().position(|l| l == "[timing]").unwrap() + 1;
        let raw = check_raw(&broken, EXAMPLE, Some(&current));
        assert_eq!(raw.problems.first().map(|(at, _)| *at), Some(Some(line)));
        assert!(raw.changes.is_empty());

        let bounded = EXAMPLE.replacen("standby_retries = 2", "standby_retries = 12", 1);
        let line = EXAMPLE
            .lines()
            .position(|l| l.starts_with("standby_retries"))
            .unwrap()
            + 1;
        let raw = check_raw(&bounded, EXAMPLE, Some(&current));
        assert_eq!(
            raw.problems,
            [(Some(line), "standby_retries must be at most 10".to_owned())]
        );

        let unknown = EXAMPLE.replacen("[timing]", "[timing]\npassword = \"x\"", 1);
        let header = EXAMPLE.lines().position(|l| l == "[timing]").unwrap() + 1;
        let raw = check_raw(&unknown, EXAMPLE, Some(&current));
        assert_eq!(
            raw.problems.first().map(|(at, _)| *at),
            Some(Some(header + 1))
        );
        assert!(
            raw.problems[0].1.contains("unknown field `password`"),
            "{:?}",
            raw.problems
        );
    }

    #[test]
    fn locate_finds_keys_in_tables_and_array_elements() {
        let line_of =
            |needle: &str| EXAMPLE.lines().position(|l| l.starts_with(needle)).unwrap() + 1;
        assert_eq!(
            locate(EXAMPLE, "timing.standby_retries"),
            Some(line_of("standby_retries"))
        );
        assert_eq!(
            locate(EXAMPLE, "device.1.media_player"),
            Some(line_of("media_player"))
        );
        assert_eq!(
            locate(EXAMPLE, "presets.slot.2"),
            Some(line_of("from = \"00:00\"") - 1)
        );
        assert_eq!(locate(EXAMPLE, "mode"), Some(1));
        assert_eq!(parse_error_position("ab\ncd", Some(4..5)), Some((2, 2)));
        assert_eq!(parse_error_position("ab", None), None);
    }

    #[test]
    fn change_sentences_use_the_table_or_say_key_old_new() {
        let before = rich();
        let after = before
            .replacen("off_delay_s = 3 ", "off_delay_s = 5 ", 1)
            .replacen("drift_grace_s = 30 ", "drift_grace_s = 45 ", 1)
            .replacen("start_wake = \"off\"", "start_wake = \"would\"", 1)
            .replacen("tv_poll_ms = 2000 ", "tv_poll_ms = 2000 # same value\n#", 1);
        let sentences: Vec<String> = changes(&before, &after)
            .into_iter()
            .map(|c| c.sentence)
            .collect();
        assert_eq!(
            sentences,
            [
                "The room waits 5 s instead of 3 s before turning off.",
                "Drift is reported after 45 s instead of 30 s of disagreement.",
                "device[console_b].probe.start_wake: \"off\" → \"would\"",
            ]
        );
        // Writing a default out, or removing a key that held its default, changes nothing.
        let written = before.replacen(
            "probe = { kind = \"ps5\",",
            "probe = { kind = \"ps5\", port = 9302,",
            1,
        );
        assert!(changes(&before, &written).is_empty());
        let dropped = before.replacen("standby_retries = 2", "", 1);
        assert!(changes(&before, &dropped).is_empty());
        let three = before.replacen("standby_retries = 2", "", 1).replacen(
            "[timing]",
            "[timing]\nstandby_retries = 3",
            1,
        );
        assert_eq!(
            changes(&before, &three)
                .into_iter()
                .map(|c| c.sentence)
                .collect::<Vec<_>>(),
            ["A displaced player gets 3 Standby retries instead of 2."]
        );
    }

    #[test]
    fn diff_marks_modified_inserted_and_deleted_lines_under_their_table() {
        let lines = |diff: Vec<DiffLine>| -> Vec<String> {
            diff.into_iter()
                .map(|line| format!("{}{}", line.sign(), line.text))
                .collect()
        };
        let before = "[timing]\na = 1\nb = 2\nc = 3\n";
        let after = "[timing]\na = 1\nb = 5\nc = 3\nd = 4\n";
        assert_eq!(
            lines(diff_lines(before, after)),
            [
                "@@ [timing] @@",
                " [timing]",
                " a = 1",
                "-b = 2",
                "+b = 5",
                " c = 3",
                "+d = 4"
            ]
        );
        let before = "x = 1\n\n[[device]]\nrole = \"kodi\"\none = 1\ntwo = 2\nthree = 3\nfour = 4\nfive = 5\nsix = 6\nseven = 7\n";
        let after = before
            .replace("one = 1\n", "")
            .replace("seven = 7", "seven = 8");
        assert_eq!(
            lines(diff_lines(before, &after)),
            [
                "@@ [[device]] role = \"kodi\" @@",
                " [[device]]",
                " role = \"kodi\"",
                "-one = 1",
                " two = 2",
                " three = 3",
                "@@ [[device]] role = \"kodi\" @@",
                " five = 5",
                " six = 6",
                "-seven = 7",
                "+seven = 8",
            ]
        );
        assert!(diff_lines(before, before).is_empty());
    }

    #[test]
    fn the_page_hash_is_what_the_runtime_compares() {
        assert_eq!(
            parse_hash(&hash(EXAMPLE)),
            Some(crate::runtime::config_hash(EXAMPLE))
        );
        assert_ne!(hash(EXAMPLE), hash(&format!("{EXAMPLE} ")));
        assert_eq!(hash(EXAMPLE).len(), 16);
        assert_eq!(parse_hash("not hex"), None);
    }

    #[test]
    fn rows_follow_the_file_then_new_rows_ignoring_repeated_and_lost_origins() {
        let form = FormData::new(
            [
                ("d.5.origin", ""),
                ("d.0.origin", "1"),
                ("d.1.origin", "0"),
                ("d.2.origin", "1"),
                ("d.3.origin", "9"),
            ]
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        );
        // `d.3` edited file row 9, which the file no longer has: it is dropped, not re-added.
        assert_eq!(row_order(&form, "d", 2), [1, 0, 5]);
    }

    #[test]
    fn the_nav_marks_restart_sections_and_the_current_state() {
        let config = Config::parse(EXAMPLE).unwrap();
        let nav = nav("timing", Some(&config), Some((2, 1)));
        let badge = |key: &str| {
            nav.iter()
                .find(|item| item.key == key)
                .unwrap()
                .badge
                .clone()
        };
        assert_eq!(badge("mqtt"), Some(("dim", "restart".to_owned())));
        assert_eq!(badge("tv"), Some(("dim", "partly".to_owned())));
        assert_eq!(badge("timing"), Some(("bad", "1 error".to_owned())));
        assert_eq!(badge("device"), Some(("plain", "4".to_owned())));
        assert!(
            nav.iter()
                .filter(|item| item.key != "timing")
                .all(|item| item.confirm.is_some())
        );
        assert_eq!(default_section(&parse(EXAMPLE)), "bridge");
    }

    #[test]
    fn secrets_are_names_only_including_the_tv_key() {
        let config = Config::parse(EXAMPLE).unwrap();
        let names: Vec<String> = secret_names(&config)
            .into_iter()
            .map(|(_, env)| env)
            .collect();
        assert!(names.contains(&"CHOREGOS_TV_KEY".to_owned()));
        assert!(names.iter().all(|name| name.starts_with("CHOREGOS_")));
        let rows = secret_rows(&config, "web.oidc");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].env, "CHOREGOS_OIDC_CLIENT_SECRET");
    }
}
