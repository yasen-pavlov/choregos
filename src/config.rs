//! Configuration parsing, validation, and environment-only secrets.

use std::{collections::HashSet, fmt, net::Ipv4Addr, path::Path};

use serde::{Deserialize, Serialize};

use crate::state::Mode;

#[derive(Clone, Default)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read configuration: {0}")]
    Io(#[from] std::io::Error),
    // Do not echo TOML source: rejected input could contain inline secrets.
    #[error("invalid TOML configuration")]
    Parse,
    #[error("invalid configuration: {0}")]
    Validation(String),
    #[error("required environment secret is missing or empty: {0}")]
    MissingSecret(String),
    #[error("cannot serialize configuration")]
    Serialize(#[from] toml::ser::Error),
    #[error("runtime is not available")]
    Unavailable,
}

#[derive(Clone, Debug, Default)]
pub struct Secrets {
    pub mqtt_password: Secret,
    pub ha_token: Secret,
    pub session_secret: Secret,
    pub oidc_client_secret: Secret,
    pub local_password: Secret,
    pub tv_http_token: Option<Secret>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub mode: Mode,
    pub mqtt: MqttConfig,
    pub bridge: BridgeConfig,
    pub home_assistant: HomeAssistantConfig,
    pub tv: TvConfig,
    pub receiver: ReceiverConfig,
    pub presets: PresetsConfig,
    pub sonos: SonosConfig,
    pub timing: TimingConfig,
    pub fake_tv: FakeTvConfig,
    pub device: Vec<DeviceConfig>,
    pub web: WebConfig,
    #[serde(skip)]
    pub secrets: Secrets,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MqttConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub client_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeConfig {
    pub topic: String,
    pub logical_address: u8,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HomeAssistantConfig {
    pub url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TvConfig {
    pub entity: String,
    pub wake: WakeConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WakeConfig {
    Wol {
        mac: String,
        broadcast: Ipv4Addr,
        port: u16,
    },
    Http {
        url: String,
        header: String,
        token_env: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiverConfig {
    pub entity: String,
    pub select_source: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PresetsConfig {
    pub timezone: String,
    pub slot: Vec<PresetSlot>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PresetSlot {
    pub from: String,
    pub to: String,
    pub script: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SonosConfig {
    pub entity: String,
    pub mute_switch: String,
    pub volume_from: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimingConfig {
    pub off_delay_s: u64,
    pub takeover_settle_ms: u64,
    pub presence_poll_s: u64,
    pub presence_misses: u32,
    pub identify_retry_s: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FakeTvConfig {
    #[serde(default = "default_true")]
    pub replies: bool,
    pub power_status: String,
    pub cec_version: String,
    pub osd_name: String,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceConfig {
    pub role: String,
    pub name: String,
    pub kind: DeviceKind,
    #[serde(rename = "match")]
    pub matches: MatchConfig,
    pub source: String,
    pub on: Vec<PowerEvent>,
    pub off: Vec<PowerEvent>,
    pub takeover: Takeover,
    pub media_player: Option<String>,
    pub presence: Presence,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Playback,
    Recording,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MatchConfig {
    pub osd: Option<String>,
    pub vendor: Option<String>,
    pub physical: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PowerEvent {
    ImageViewOn,
    TextViewOn,
    ActiveSource,
    PowerOn,
    Standby,
    InactiveSource,
    PowerStandby,
    ReportPhysicalAddress,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Takeover {
    CecStandby,
    HaMediaStop,
    None,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    Events,
    Poll,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    pub bind: std::net::SocketAddr,
    pub public_url: String,
    pub oidc: OidcConfig,
    pub local_login: LocalLoginConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalLoginConfig {
    pub username: String,
}

pub fn load(path: impl AsRef<Path>) -> Result<(Config, String), ConfigError> {
    let text = std::fs::read_to_string(path)?;
    let mut config = Config::parse(&text)?;
    config.resolve_secrets(|name| std::env::var(name).ok())?;
    Ok((config, text))
}

impl Config {
    /// Parse and validate without reading the environment (used by config editing).
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text).map_err(|_| ConfigError::Parse)?;
        config.validate()?;
        Ok(config)
    }

    pub fn resolve_secrets(
        &mut self,
        mut lookup: impl FnMut(&str) -> Option<String>,
    ) -> Result<(), ConfigError> {
        let mut required = |name: &str| {
            lookup(name)
                .filter(|value| !value.is_empty())
                .map(Secret)
                .ok_or_else(|| ConfigError::MissingSecret(name.to_owned()))
        };
        let secrets = Secrets {
            mqtt_password: required("CHOREGOS_MQTT_PASSWORD")?,
            ha_token: required("CHOREGOS_HA_TOKEN")?,
            session_secret: required("CHOREGOS_SESSION_SECRET")?,
            oidc_client_secret: required("CHOREGOS_OIDC_CLIENT_SECRET")?,
            local_password: required("CHOREGOS_LOCAL_PASSWORD")?,
            tv_http_token: match &self.tv.wake {
                WakeConfig::Http { token_env, .. } => Some(required(token_env)?),
                WakeConfig::Wol { .. } => None,
            },
        };
        if secrets.session_secret.expose().len() < 32 {
            return invalid("CHOREGOS_SESSION_SECRET must contain at least 32 bytes");
        }
        self.secrets = secrets;
        Ok(())
    }

    pub fn redacted_text(&self) -> Result<String, ConfigError> {
        let mut text = toml::to_string_pretty(self)?;
        text.push_str("\n# Environment secrets (resolved values are never serialized):\n");
        for name in [
            "CHOREGOS_MQTT_PASSWORD",
            "CHOREGOS_HA_TOKEN",
            "CHOREGOS_SESSION_SECRET",
            "CHOREGOS_OIDC_CLIENT_SECRET",
            "CHOREGOS_LOCAL_PASSWORD",
        ] {
            text.push_str(&format!("# {name} = [REDACTED]\n"));
        }
        if let WakeConfig::Http { .. } = self.tv.wake {
            text.push_str("# TV HTTP token = [REDACTED]\n");
        }
        Ok(text)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_devices()?;
        self.validate_timing()?;
        self.validate_fake_tv()?;
        if self.bridge.logical_address > 14 {
            return invalid("bridge logical address must be in 0..=14");
        }
        if let WakeConfig::Wol { mac, .. } = &self.tv.wake {
            parse_mac(mac)?;
        }
        self.validate_presets()
    }

    fn validate_devices(&self) -> Result<(), ConfigError> {
        let mut roles = HashSet::new();
        let mut sources = HashSet::new();
        for device in &self.device {
            if device.role.is_empty() || !roles.insert(device.role.as_str()) {
                return invalid("device roles must be nonempty and unique");
            }
            if !device
                .role
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
                || ["bridge", "mode", "conflicts", "active_source", "all_off"]
                    .contains(&device.role.as_str())
            {
                return invalid(
                    "device roles must use [a-z0-9_]+ and cannot use reserved entity names",
                );
            }
            if device.source.is_empty() || !sources.insert(device.source.as_str()) {
                return invalid("device sources must be nonempty and unique");
            }
            device.validate_matching_and_policy()?;
        }
        Ok(())
    }

    fn validate_timing(&self) -> Result<(), ConfigError> {
        if self.timing.presence_poll_s == 0
            || self.timing.presence_misses == 0
            || self.timing.identify_retry_s == 0
        {
            return invalid(
                "presence_poll_s, presence_misses and identify_retry_s must be positive",
            );
        }
        Ok(())
    }

    fn validate_fake_tv(&self) -> Result<(), ConfigError> {
        if self.fake_tv.power_status != "always_on" || self.fake_tv.cec_version != "1.4" {
            return invalid("fake_tv supports only always_on power and CEC version 1.4");
        }
        if self.fake_tv.osd_name.is_empty()
            || self.fake_tv.osd_name.len() > 14
            || !self.fake_tv.osd_name.is_ascii()
        {
            return invalid("fake_tv.osd_name must contain 1–14 ASCII bytes");
        }
        Ok(())
    }

    fn validate_presets(&self) -> Result<(), ConfigError> {
        if self.presets.timezone.parse::<chrono_tz::Tz>().is_err() {
            return invalid("presets timezone must be an IANA timezone");
        }
        let mut intervals = self
            .presets
            .slot
            .iter()
            .map(|slot| Ok((parse_time(&slot.from, false)?, parse_time(&slot.to, true)?)))
            .collect::<Result<Vec<_>, ConfigError>>()?;
        intervals.sort_unstable();
        let mut cursor = 0;
        for (from, to) in intervals {
            if from != cursor || to <= from {
                return invalid("preset slots must cover 24 hours without gaps or overlap");
            }
            cursor = to;
        }
        if cursor != 24 * 60 {
            return invalid("preset slots must cover 24 hours without gaps or overlap");
        }
        Ok(())
    }
}

impl DeviceConfig {
    fn validate_matching_and_policy(&self) -> Result<(), ConfigError> {
        let rule = &self.matches;
        if rule.osd.as_ref().is_none_or(|osd| osd.trim().is_empty())
            && rule.vendor.is_none()
            && rule.physical.is_none()
        {
            return invalid("each device requires at least one nonempty match key");
        }
        if let Some(vendor) = &rule.vendor
            && (vendor.len() != 6 || !vendor.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return invalid("vendor must be exactly six hexadecimal digits");
        }
        if let Some(physical) = &rule.physical {
            parse_physical(physical)?;
        }
        if self.presence == Presence::Poll && self.kind != DeviceKind::Playback {
            return invalid("poll presence requires playback kind");
        }
        if self.takeover == Takeover::HaMediaStop && self.media_player.is_none() {
            return invalid("ha_media_stop takeover requires media_player");
        }
        Ok(())
    }
}

fn invalid<T>(message: &str) -> Result<T, ConfigError> {
    Err(ConfigError::Validation(message.to_owned()))
}

fn parse_time(value: &str, allow_end: bool) -> Result<u16, ConfigError> {
    let bytes = value.as_bytes();
    if bytes.len() != 5
        || bytes[2] != b':'
        || ![bytes[0], bytes[1], bytes[3], bytes[4]]
            .iter()
            .all(u8::is_ascii_digit)
    {
        return invalid("preset times must use HH:MM");
    }
    let hours = u16::from(bytes[0] - b'0') * 10 + u16::from(bytes[1] - b'0');
    let minutes = u16::from(bytes[3] - b'0') * 10 + u16::from(bytes[4] - b'0');
    if minutes >= 60 || hours > 24 || (hours == 24 && (!allow_end || minutes != 0)) {
        return invalid("preset times must be within 00:00..24:00");
    }
    Ok(hours * 60 + minutes)
}

pub fn parse_mac(value: &str) -> Result<[u8; 6], ConfigError> {
    let mut mac = [0; 6];
    let mut parts = value.split(':');
    for byte in &mut mac {
        let Some(part) = parts.next() else {
            return invalid("MAC must contain six colon-separated hex octets");
        };
        if part.len() != 2 || !part.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return invalid("MAC must contain six colon-separated hex octets");
        }
        *byte = u8::from_str_radix(part, 16)
            .map_err(|_| ConfigError::Validation("invalid MAC octet".to_owned()))?;
    }
    if parts.next().is_some() {
        return invalid("MAC must contain six colon-separated hex octets");
    }
    Ok(mac)
}

pub fn parse_physical(value: &str) -> Result<u16, ConfigError> {
    let mut parts = value.split('.');
    let mut address = 0;
    for _ in 0..4 {
        let Some(part) = parts.next().filter(|part| part.len() == 1) else {
            return invalid("physical address must contain four dotted hexadecimal digits");
        };
        let digit = u16::from_str_radix(part, 16)
            .map_err(|_| ConfigError::Validation("invalid physical address digit".to_owned()))?;
        address = (address << 4) | digit;
    }
    if parts.next().is_some() {
        return invalid("physical address must contain four dotted hexadecimal digits");
    }
    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Config {
        Config::parse(include_str!("../choregos.example.toml")).expect("valid example")
    }

    #[test]
    fn rejects_identity_ambiguities_and_empty_matching() {
        let mut config = example();
        config.device[1].role = config.device[0].role.clone();
        assert!(config.validate().is_err());
        let mut config = example();
        config.device[1].source = config.device[0].source.clone();
        assert!(config.validate().is_err());
        let mut config = example();
        config.device[0].matches = MatchConfig {
            osd: None,
            vendor: None,
            physical: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_poll_for_recording_and_bad_network_addresses() {
        let mut config = example();
        config.device[1].presence = Presence::Poll;
        assert!(config.validate().is_err());
        assert!(parse_mac("AA:BB:CC:DD:EE").is_err());
        assert!(parse_mac("AA:BB:CC:DD:EE:GG").is_err());
        assert!(parse_mac("AA:BB:CC:DD:EE:FF:00").is_err());
        assert!(parse_mac("+A:BB:CC:DD:EE:FF").is_err());
        let text = include_str!("../choregos.example.toml");
        assert!(Config::parse(&text.replace("192.168.10.255", "::1")).is_err());
        assert!(Config::parse(&text.replace("AA:BB:CC:DD:EE:FF", "bad")).is_err());
    }

    #[test]
    fn requires_exact_24_hour_coverage() {
        let mut config = example();
        config.presets.slot[0].from = "09:01".into();
        assert!(config.validate().is_err());
        config.presets.slot[0].from = "08:59".into();
        assert!(config.validate().is_err());
        config.presets.slot[0].from = "24:00".into();
        assert!(config.validate().is_err());
        config.presets.slot.clear();
        assert!(config.validate().is_err());
        assert!(parse_time("09:60", true).is_err());
        assert!(parse_time("9:00", true).is_err());
    }

    #[test]
    fn secrets_are_required_and_redacted() {
        let mut config = example();
        assert!(config.resolve_secrets(|_| None).is_err());
        assert!(config.resolve_secrets(|_| Some("short".into())).is_err());
        let sentinel = "test-only-secret-not-a-real-credential";
        config
            .resolve_secrets(|_| Some(sentinel.into()))
            .expect("dummy secrets");
        assert!(!config.redacted_text().unwrap().contains(sentinel));
        assert!(!format!("{config:?}").contains(sentinel));
        assert_eq!(config.secrets.ha_token.expose(), sentinel);
    }

    #[test]
    fn http_wake_resolves_its_own_environment_secret() {
        let mut config = example();
        config.tv.wake = WakeConfig::Http {
            url: "https://wake.example.com".into(),
            header: "Authorization".into(),
            token_env: "TEST_HTTP_TOKEN".into(),
        };
        assert!(
            config
                .resolve_secrets(|name| (name != "TEST_HTTP_TOKEN").then(|| "x".repeat(32)))
                .is_err()
        );
        config.resolve_secrets(|_| Some("x".repeat(32))).unwrap();
        assert!(config.secrets.tv_http_token.is_some());
    }

    #[test]
    fn rejects_inline_secrets_without_echoing_input() {
        let text = include_str!("../choregos.example.toml")
            .replace("[mqtt]", "[mqtt]\npassword = \"forbidden-inline-secret\"");
        let error = Config::parse(&text).unwrap_err();
        assert!(!error.to_string().contains("forbidden-inline-secret"));
        assert!(!format!("{error:?}").contains("forbidden-inline-secret"));
        assert!(std::error::Error::source(&error).is_none());
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;
    #[test]
    fn rejects_reserved_and_non_entity_role_ids() {
        for role in [
            "bridge",
            "mode",
            "conflicts",
            "active_source",
            "all_off",
            "TV",
            "a/b",
            "a+",
            "a#",
            "two words",
            "",
        ] {
            let mut config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
            config.device[0].role = role.into();
            assert!(config.validate().is_err(), "accepted {role}");
        }
    }
    #[test]
    fn directed_replies_default_on_and_can_be_disabled() {
        let example = include_str!("../choregos.example.toml");
        assert!(
            Config::parse(&example.replace("replies = true", "# replies use default"))
                .unwrap()
                .fake_tv
                .replies
        );
        assert!(
            !Config::parse(&example.replace("replies = true", "replies = false"))
                .unwrap()
                .fake_tv
                .replies
        );
    }
}
