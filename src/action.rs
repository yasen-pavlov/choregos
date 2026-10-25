use crate::state::RolePower;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    Identify {
        la: u8,
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
