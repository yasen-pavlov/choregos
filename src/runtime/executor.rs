use super::{ha::Ha, mqtt::Mqtt, webos, wol};
#[cfg(test)]
use crate::action::IdentifyQueries;
use crate::{
    action::Action,
    config::{Config, TvOffConfig, WakeConfig},
    state::Mode,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub struct Executor {
    pub mqtt: Mqtt,
    pub ha: Ha,
    pub active: Arc<AtomicBool>,
}
impl Executor {
    fn permits(&self, action: &Action, config: &Config) -> bool {
        let mode = if self.active.load(Ordering::Acquire) {
            config.mode
        } else {
            Mode::Shadow
        };
        if suppressed(mode, action) {
            tracing::info!(action = %serde_json::to_string(action).expect("serializable action"), "would: execute action");
            false
        } else {
            true
        }
    }

    /// Immediate, non-blocking CEC lane. None means this action belongs to the ordered worker.
    pub fn fast_lane(
        &self,
        action: &Action,
        config: &Config,
        known_las: &[u8],
    ) -> Option<Vec<(crate::cec::Frame, Option<String>)>> {
        if matches!(
            action,
            Action::WakeTv | Action::TvOff | Action::HaCall { .. }
        ) {
            return None;
        }
        if !self.permits(action, config) {
            return Some(vec![]);
        }
        let mut frames = vec![];
        let mut send = |to, data: &[u8]| {
            if let Some(id) = self.mqtt.send(to, data) {
                frames.push((
                    crate::cec::Frame {
                        from: config.bridge.logical_address(),
                        to,
                        opcode: data.first().copied(),
                        params: data.get(1..).unwrap_or_default().to_vec(),
                        ack: None,
                        t_us: None,
                    },
                    id,
                ));
            }
        };
        match action {
            Action::Identify { la, queries } => {
                if *la < 15 && *la != config.bridge.logical_address() {
                    for (needed, opcode) in [
                        (queries.physical, 0x83),
                        (queries.vendor, 0x8c),
                        (queries.osd, 0x46),
                    ] {
                        if needed {
                            send(*la, &[opcode]);
                        }
                    }
                }
            }
            Action::PresencePoll { la } => send(*la, &[0x8f]),
            Action::Resync => {
                send(15, &[0x85]);
                let mut targets: u16 = (1 << 1) | (1 << 4) | (1 << 8) | (1 << 11);
                for &la in known_las {
                    if la < 15 {
                        targets |= 1 << la;
                    }
                }
                for la in 0..15 {
                    if targets & (1 << la) != 0 {
                        send(la, &[0x8f]);
                    }
                }
            }
            Action::CecSend { to, data } => send(*to, data),
            Action::WakeTv | Action::TvOff | Action::HaCall { .. } => {
                unreachable!("ordered action checked above")
            }
        }
        Some(frames)
    }
    pub async fn execute(&self, action: &Action, config: &Config) -> anyhow::Result<()> {
        if !self.permits(action, config) {
            return Ok(());
        }
        match action {
            Action::Identify { .. }
            | Action::Resync
            | Action::PresencePoll { .. }
            | Action::CecSend { .. } => {
                anyhow::bail!("CEC action incorrectly sent to the ordered worker");
            }
            Action::HaCall {
                domain,
                service,
                entity,
                data,
            } => self.ha.call(domain, service, entity, data.clone()).await?,
            Action::TvOff => match &config.tv.off {
                TvOffConfig::Ha => {
                    self.ha
                        .call(
                            "media_player",
                            "turn_off",
                            &config.tv.entity,
                            serde_json::json!({}),
                        )
                        .await?;
                }
                TvOffConfig::Webos { host, port, .. } => {
                    let key = config
                        .secrets
                        .tv_webos_key
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("missing webOS power-off credential"))?;
                    retry_webos_off(|| webos::turn_off(host, *port, key)).await?;
                }
            },
            Action::WakeTv => match &config.tv.wake {
                WakeConfig::Wol {
                    mac,
                    broadcast,
                    port,
                } => wol::wake(mac, *broadcast, *port).await?,
                WakeConfig::Http { url, header, .. } => {
                    let token = config
                        .secrets
                        .tv_http_token
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("missing HTTP wake credential"))?;
                    // Never propagate reqwest errors containing a URL or authorization material.
                    openidconnect::reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(10))
                        .build()?
                        .post(url)
                        .header(header, token.expose())
                        .send()
                        .await
                        .map_err(|_| anyhow::anyhow!("HTTP wake request failed"))?
                        .error_for_status()
                        .map_err(|_| anyhow::anyhow!("HTTP wake returned an error status"))?;
                }
            },
        }
        Ok(())
    }
}

async fn retry_webos_off<F, Fut>(mut operation: F) -> anyhow::Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    for attempt in 1..=3 {
        match operation().await {
            Ok(()) => return Ok(()),
            Err(error) => {
                tracing::warn!(attempt, error = %error, "webOS power-off attempt failed");
                if error.is::<webos::PairingRequired>() || attempt == 3 {
                    tracing::error!(attempt, error = %error, "webOS power-off failed");
                    return Err(error);
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
    unreachable!("last attempt returns its result")
}
pub fn suppressed(mode: Mode, action: &Action) -> bool {
    mode == Mode::Shadow && !action.allowed_in_shadow()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn pairing_required_is_attempted_once_without_retry_delay() {
        let mut attempts = 0;
        let error = retry_webos_off(|| {
            attempts += 1;
            std::future::ready(Err(webos::PairingRequired.into()))
        })
        .await
        .unwrap_err();
        assert!(error.is::<webos::PairingRequired>());
        assert_eq!(attempts, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn transient_failures_retry_three_times_one_second_apart() {
        let start = tokio::time::Instant::now();
        let mut attempts = 0;
        let error = retry_webos_off(|| {
            attempts += 1;
            std::future::ready(Err(anyhow::anyhow!("test-only connection failure")))
        })
        .await
        .unwrap_err();
        assert_eq!(attempts, 3);
        assert_eq!(
            tokio::time::Instant::now() - start,
            std::time::Duration::from_secs(2)
        );
        assert_eq!(error.to_string(), "test-only connection failure");
    }

    #[test]
    fn shadow_gate_covers_every_effect() {
        let effects = [
            Action::WakeTv,
            Action::TvOff,
            Action::ha("media_player", "turn_off", "media_player.example"),
            Action::CecSend {
                to: 4,
                data: vec![0x36],
            },
        ];
        for effect in effects {
            assert!(suppressed(Mode::Shadow, &effect));
            assert!(!suppressed(Mode::Active, &effect));
        }
        assert!(!suppressed(
            Mode::Shadow,
            &Action::Identify {
                la: 4,
                queries: IdentifyQueries::ALL
            }
        ));
        assert!(!suppressed(Mode::Shadow, &Action::Resync));
    }
}

#[cfg(test)]
mod behavior_tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[tokio::test]
    async fn actual_effects_obey_produced_and_current_modes_for_every_action_class() {
        for (produced_mode, live_active, executes) in [
            (Mode::Active, true, true),
            (Mode::Shadow, true, false),
            (Mode::Active, false, false),
            (Mode::Shadow, false, false),
        ] {
            let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
            config.mode = produced_mode;
            config.tv.off = TvOffConfig::Ha;
            let udp = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            config.tv.wake = WakeConfig::Wol {
                mac: "02:11:22:33:44:55".into(),
                broadcast: Ipv4Addr::LOCALHOST,
                port: udp.local_addr().unwrap().port(),
            };
            let (mqtt, mut eventloop) = Mqtt::test_transport(true, 32);
            let (ha, mut calls) = Ha::fake();
            let executor = Executor {
                mqtt,
                ha,
                active: Arc::new(AtomicBool::new(live_active)),
            };
            for (action, count) in [
                (
                    Action::Identify {
                        la: 4,
                        queries: IdentifyQueries::ALL,
                    },
                    3,
                ),
                (Action::Resync, 5),
                (Action::PresencePoll { la: 8 }, 1),
                (
                    Action::CecSend {
                        to: 4,
                        data: vec![0x36],
                    },
                    usize::from(executes),
                ),
            ] {
                let frames = executor.fast_lane(&action, &config, &[]).unwrap();
                assert_eq!(frames.len(), count);
            }
            executor
                .execute(
                    &Action::ha("media_player", "turn_on", "media_player.example"),
                    &config,
                )
                .await
                .unwrap();
            executor.execute(&Action::WakeTv, &config).await.unwrap();
            assert!(executor.fast_lane(&Action::TvOff, &config, &[]).is_none());
            executor.execute(&Action::TvOff, &config).await.unwrap();
            eventloop.clean();
            assert_eq!(eventloop.pending.len(), 9 + usize::from(executes));
            if executes {
                assert_eq!(calls.recv().await.unwrap()["service"], "turn_on");
                assert_eq!(
                    calls.recv().await.unwrap(),
                    serde_json::json!({
                        "domain": "media_player",
                        "service": "turn_off",
                        "entity": config.tv.entity,
                        "data": {}
                    })
                );
                let mut packet = [0; 102];
                tokio::time::timeout(std::time::Duration::from_secs(1), udp.recv(&mut packet))
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(packet, wol::magic_packet("02:11:22:33:44:55").unwrap());
            } else {
                assert!(calls.try_recv().is_err());
                assert!(udp.try_recv(&mut [0; 102]).is_err());
            }
        }
    }

    #[tokio::test]
    async fn disconnected_fast_lane_drops_every_cec_class_without_waiting() {
        let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        config.mode = Mode::Active;
        let (mqtt, mut eventloop) = Mqtt::test_transport(false, 1);
        let (ha, _) = Ha::fake();
        let executor = Executor {
            mqtt,
            ha,
            active: Arc::new(AtomicBool::new(true)),
        };
        for action in [
            Action::Identify {
                la: 4,
                queries: IdentifyQueries::ALL,
            },
            Action::Resync,
            Action::PresencePoll { la: 8 },
            Action::CecSend {
                to: 4,
                data: vec![0x90, 0],
            },
        ] {
            assert!(
                executor
                    .fast_lane(&action, &config, &[])
                    .unwrap()
                    .is_empty()
            );
        }
        eventloop.clean();
        assert!(eventloop.pending.is_empty());
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;
    #[tokio::test]
    async fn http_wake_uses_environment_header_and_redacts_failure_details() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = axum::Router::new()
            .route(
                "/wake",
                axum::routing::post(|headers: axum::http::HeaderMap| async move {
                    assert_eq!(
                        headers["Authorization"],
                        "test-only-dummy-credential-value-32"
                    );
                    axum::http::StatusCode::NO_CONTENT
                }),
            )
            .route(
                "/failure",
                axum::routing::post(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        config.mode = Mode::Active;
        config.tv.wake = WakeConfig::Http {
            url: format!("http://{address}/wake"),
            header: "Authorization".into(),
            token_env: "DUMMY_WAKE_TOKEN".into(),
        };
        config
            .resolve_secrets(|_| Some("test-only-dummy-credential-value-32".into()))
            .unwrap();
        let (mqtt, _) = Mqtt::test_transport(false, 1);
        let (ha, _) = Ha::fake();
        let executor = Executor {
            mqtt,
            ha,
            active: Arc::new(AtomicBool::new(true)),
        };
        executor.execute(&Action::WakeTv, &config).await.unwrap();
        if let WakeConfig::Http { url, .. } = &mut config.tv.wake {
            *url = format!("http://{address}/failure");
        }
        let error = executor
            .execute(&Action::WakeTv, &config)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "HTTP wake returned an error status");
        assert!(!error.contains(&address.to_string()));
        server.abort();
    }
}
