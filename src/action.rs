use crate::state::RolePower;
use serde::Serialize;

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

#[derive(Clone, Debug, Serialize)]
pub struct Decision {
    pub role: Option<String>,
    pub from: Option<RolePower>,
    pub to: Option<RolePower>,
    pub reason: String,
    pub actions: Vec<Action>,
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
