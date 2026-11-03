use crate::state::{Command, RolePower};
use serde::{Deserialize, Serialize};

/// Read-only identity queries, selected independently for each missing key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct IdentifyQueries {
    pub physical: bool,
    pub vendor: bool,
    pub osd: bool,
}

impl IdentifyQueries {
    pub const ALL: Self = Self {
        physical: true,
        vendor: true,
        osd: true,
    };

    pub fn is_empty(self) -> bool {
        !self.physical && !self.vendor && !self.osd
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    Identify {
        la: u8,
        queries: IdentifyQueries,
    },
    Resync,
    PresencePoll {
        la: u8,
    },
    CecSend {
        to: u8,
        data: Vec<u8>,
    },
    WakeTv,
    TvOff,
    HaCall {
        domain: String,
        service: String,
        entity: String,
        data: serde_json::Value,
    },
}

impl Action {
    pub fn ha(domain: &str, service: &str, entity: &str) -> Self {
        Self::HaCall {
            domain: domain.into(),
            service: service.into(),
            entity: entity.into(),
            data: serde_json::json!({}),
        }
    }
    pub fn allowed_in_shadow(&self) -> bool {
        matches!(
            self,
            Self::Identify { .. } | Self::Resync | Self::PresencePoll { .. }
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TimerKind {
    RoomOff,
    TakeoverSettle,
    Presence,
    StandbyVerification,
    Warmup,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServiceKind {
    Started,
    Migrated,
}

/// The input or timer responsible for a decision, independent of its outcome.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    Frame { from: u8, to: u8, data: String },
    Command { command: Command },
    Timer { name: TimerKind },
    Service { name: ServiceKind },
}

/// One observable effect of a decision. Values stay strings across adapters.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Expectation {
    pub subject: String,
    pub field: Field,
    pub expect: String,
    pub via: Vec<Via>,
    pub deadline_ms: u64,
    pub attempts: u8,
    pub outcome: Outcome,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    Power,
    Source,
    Playback,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    TvDirect,
    Ps5,
    Ha,
    Cec,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Pending,
    Met {
        via: Via,
        observed: String,
        latency_ms: u64,
    },
    Missed {
        observed: String,
    },
    Unknown {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed: Option<String>,
    },
    CecOnly {
        observed: String,
    },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Pending,
    Achieved,
    Partial,
    Missed,
    Unknown,
    #[default]
    None,
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Pending => "pending",
            Self::Achieved => "achieved",
            Self::Partial => "partial",
            Self::Missed => "missed",
            Self::Unknown => "unknown",
            Self::None => "none",
        })
    }
}

impl std::fmt::Display for Field {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Power => "power",
            Self::Source => "source",
            Self::Playback => "playback",
        })
    }
}

impl std::fmt::Display for Via {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TvDirect => "tv_direct",
            Self::Ps5 => "ps5",
            Self::Ha => "ha",
            Self::Cec => "cec",
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct VerdictUpdate {
    pub id: u64,
    pub verdict: Verdict,
    pub expectations: Vec<Expectation>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Decision {
    pub id: u64,
    pub parent: Option<u64>,
    pub trigger: Trigger,
    pub role: Option<String>,
    pub from: Option<RolePower>,
    pub to: Option<RolePower>,
    pub reason: String,
    pub actions: Vec<Action>,
    pub expectations: Vec<Expectation>,
    pub verdict: Verdict,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tv_off_has_a_redacted_transport_independent_action() {
        let serialized = serde_json::to_string(&Action::TvOff).unwrap();
        assert_eq!(serialized, r#"{"kind":"tv_off"}"#);
        assert!(!Action::TvOff.allowed_in_shadow());
        println!("{serialized}");
    }
}
