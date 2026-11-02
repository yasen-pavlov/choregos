//! Action-scoped, read-only polling. Observations never trigger remediation.
use super::{Input, probe, webos};
use crate::{
    config::{Config, ProbeConfig, TvOffConfig},
    engine::Event,
    state::{ConsoleState, Mode, TvState},
};
use chrono::Utc;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinHandle, time::Instant};

pub(super) struct Observer {
    tx: mpsc::Sender<Input>,
    active: Arc<AtomicBool>,
    pairing_blocked: Arc<AtomicBool>,
    task: Mutex<Option<JoinHandle<()>>>,
    consoles: Mutex<BTreeMap<String, (ConsoleState, JoinHandle<()>)>>,
}

impl Observer {
    pub fn new(tx: mpsc::Sender<Input>, active: Arc<AtomicBool>) -> Self {
        Self {
            tx,
            active,
            pairing_blocked: Arc::new(AtomicBool::new(false)),
            task: Mutex::new(None),
            consoles: Mutex::new(BTreeMap::new()),
        }
    }

    /// Called only after the ordered executor has dispatched the TV action.
    pub fn tv(&self, config: Arc<Config>, target: TvState) {
        if config.mode != Mode::Active
            || !self.active.load(Ordering::Acquire)
            || self.pairing_blocked.load(Ordering::Acquire)
        {
            return;
        }
        let TvOffConfig::Webos { .. } = &config.tv.off else {
            return;
        };
        let Some(_) = config.secrets.tv_webos_key.as_ref() else {
            return;
        };
        let active = self.active.clone();
        let blocked = self.pairing_blocked.clone();
        let tx = self.tx.clone();
        self.replace(async move {
            poll_window(
                &tx,
                &active,
                config.verify.tv_poll_ms,
                config.verify.tv_window_s,
                || async {
                    let TvOffConfig::Webos { host, port, .. } = &config.tv.off else {
                        unreachable!()
                    };
                    let key = config
                        .secrets
                        .tv_webos_key
                        .as_ref()
                        .expect("checked before spawning");
                    let state = match webos::power_state(host, *port, key).await {
                        Ok(state) => state,
                        Err(error) if error.is::<webos::PairingRequired>() => {
                            if !blocked.swap(true, Ordering::AcqRel) {
                                tracing::warn!(
                                    "direct TV observation stopped: webOS pairing required"
                                );
                            }
                            return Err(error);
                        }
                        Err(_) => TvState::Unreachable,
                    };
                    Ok((
                        Event::TvPower {
                            state,
                            at: Utc::now(),
                        },
                        state == target,
                    ))
                },
            )
            .await;
        });
    }

    fn replace(&self, operation: impl Future<Output = ()> + Send + 'static) {
        let mut task = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(previous) = task.take() {
            previous.abort();
        }
        *task = Some(tokio::spawn(operation));
    }

    pub fn console(&self, config: Arc<Config>, role: &str, target: ConsoleState) {
        if config.mode != Mode::Active || !self.active.load(Ordering::Acquire) {
            return;
        }
        let Some(probe) = config
            .device
            .iter()
            .find(|device| device.role == role)
            .and_then(|device| device.probe.clone())
        else {
            return;
        };
        let mut consoles = self
            .consoles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((previous, task)) = consoles.get(role)
            && *previous == target
            && !task.is_finished()
        {
            return;
        }
        if let Some((_, task)) = consoles.remove(role) {
            task.abort();
        }
        let tx = self.tx.clone();
        let active = self.active.clone();
        let role_name = role.to_owned();
        let task = tokio::spawn(async move {
            let ProbeConfig::Ps5 { host, port } = probe;
            poll_window(
                &tx,
                &active,
                config.verify.probe_poll_ms,
                config.verify.probe_window_s,
                || async {
                    let state = probe::power_state(&host, port).await;
                    Ok((
                        Event::ConsolePower {
                            role: role_name.clone(),
                            state,
                            at: Utc::now(),
                        },
                        state == target,
                    ))
                },
            )
            .await;
        });
        consoles.insert(role.to_owned(), (target, task));
    }

    pub fn stop(&self) {
        if let Some(task) = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
        for (_, (_, task)) in std::mem::take(
            &mut *self
                .consoles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        ) {
            task.abort();
        }
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Injectable query future keeps scheduling tests independent of TLS and hardware.
async fn poll_window<F, Fut>(
    tx: &mpsc::Sender<Input>,
    active: &AtomicBool,
    poll_ms: u64,
    window_s: u64,
    mut query: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<(Event, bool)>>,
{
    let deadline = Instant::now() + Duration::from_secs(window_s);
    let mut interval = tokio::time::interval(Duration::from_millis(poll_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        if !active.load(Ordering::Acquire) || Instant::now() >= deadline {
            break;
        }
        if tokio::time::timeout_at(deadline, interval.tick())
            .await
            .is_err()
        {
            break;
        }
        if !active.load(Ordering::Acquire) || Instant::now() >= deadline {
            break;
        }
        let Ok(Ok((event, reached))) = tokio::time::timeout_at(deadline, query()).await else {
            break;
        };
        if !active.load(Ordering::Acquire) {
            break;
        }
        if tokio::time::timeout_at(deadline, tx.send(Input::Event(event)))
            .await
            .is_err()
        {
            break;
        }
        if reached || tx.is_closed() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn live_config(port: u16) -> Arc<Config> {
        let mut config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        config.mode = Mode::Active;
        config.tv.off = TvOffConfig::Webos {
            host: "127.0.0.1".into(),
            port,
            key_env: "CHOREGOS_TEST_TV".into(),
        };
        config
            .resolve_secrets(|_| Some("observation-test-only-secret-value".into()))
            .unwrap();
        Arc::new(config)
    }

    async fn accept_in_flight_query(listener: &tokio::net::TcpListener) -> tokio::net::TcpStream {
        use tokio::io::AsyncReadExt;
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut first_byte = [0; 1];
        tokio::time::timeout(Duration::from_secs(1), socket.read_exact(&mut first_byte))
            .await
            .unwrap()
            .unwrap();
        socket
    }

    async fn assert_query_cancelled(mut socket: tokio::net::TcpStream) {
        use tokio::io::AsyncReadExt;
        let mut remainder = vec![];
        // The SSAP operation has a five-second timeout; EOF within one second
        // establishes cancellation rather than ordinary request expiry.
        tokio::time::timeout(Duration::from_secs(1), socket.read_to_end(&mut remainder))
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn newer_tv_action_replaces_the_in_flight_observation_window() {
        let old = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let new = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let observer = Observer::new(tx, Arc::new(AtomicBool::new(true)));
        observer.tv(live_config(old.local_addr().unwrap().port()), TvState::On);
        let first = accept_in_flight_query(&old).await;
        observer.tv(live_config(new.local_addr().unwrap().port()), TvState::Off);
        assert_query_cancelled(first).await;
        assert!(
            rx.try_recv().is_err(),
            "cancelled query must not publish Unreachable"
        );
        let second = accept_in_flight_query(&new).await;
        drop(second);
        let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            event,
            Input::Event(Event::TvPower {
                state: TvState::Unreachable,
                ..
            })
        ));
        observer.stop();
    }

    #[tokio::test]
    async fn shutdown_cancels_an_in_flight_window_without_publishing_a_sample() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let observer = Observer::new(tx, Arc::new(AtomicBool::new(true)));
        observer.tv(
            live_config(listener.local_addr().unwrap().port()),
            TvState::On,
        );
        let socket = accept_in_flight_query(&listener).await;
        observer.stop();
        assert_query_cancelled(socket).await;
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn console_windows_replace_targets_and_cancel_in_flight_on_shutdown() {
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let replacement = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut config = (*live_config(3001)).clone();
        config.device[0].probe = Some(ProbeConfig::Ps5 {
            host: "127.0.0.1".into(),
            port: server.local_addr().unwrap().port(),
        });
        let config = Arc::new(config);
        let (tx, mut rx) = mpsc::channel(8);
        let observer = Observer::new(tx, Arc::new(AtomicBool::new(true)));
        let mut request = [0; 128];
        observer.console(config.clone(), "streamer", ConsoleState::Awake);
        let (_, old_sender) =
            tokio::time::timeout(Duration::from_secs(1), server.recv_from(&mut request))
                .await
                .unwrap()
                .unwrap();
        let old = observer
            .consoles
            .lock()
            .unwrap()
            .get("streamer")
            .unwrap()
            .1
            .abort_handle();
        let mut replacement_config = (*config).clone();
        replacement_config.device[0].probe = Some(ProbeConfig::Ps5 {
            host: "127.0.0.1".into(),
            port: replacement.local_addr().unwrap().port(),
        });
        let config = Arc::new(replacement_config);
        observer.console(config.clone(), "streamer", ConsoleState::Standby);
        let (_, new_sender) =
            tokio::time::timeout(Duration::from_secs(1), replacement.recv_from(&mut request))
                .await
                .unwrap()
                .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !old.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        server
            .send_to(b"HTTP/1.1 200 Ok\n", old_sender)
            .await
            .unwrap();
        replacement
            .send_to(b"HTTP/1.1 620 Server Standby\n", new_sender)
            .await
            .unwrap();
        let sample = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            sample,
            Input::Event(Event::ConsolePower {
                state: ConsoleState::Standby,
                ..
            })
        ));
        observer.console(config, "streamer", ConsoleState::Awake);
        let (_, sender) =
            tokio::time::timeout(Duration::from_secs(1), replacement.recv_from(&mut request))
                .await
                .unwrap()
                .unwrap();
        let pending = observer
            .consoles
            .lock()
            .unwrap()
            .get("streamer")
            .unwrap()
            .1
            .abort_handle();
        observer.stop();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !pending.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        replacement
            .send_to(b"HTTP/1.1 200 Ok\n", sender)
            .await
            .unwrap();
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn stops_on_target_at_poll_cadence() {
        let (tx, mut rx) = mpsc::channel(8);
        let active = AtomicBool::new(true);
        let calls = AtomicUsize::new(0);
        let start = Instant::now();
        poll_window(&tx, &active, 2000, 45, || {
            let call = calls.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Ok((
                Event::TvPower {
                    state: if call == 2 {
                        TvState::On
                    } else {
                        TvState::Unreachable
                    },
                    at: Utc::now(),
                },
                call == 2,
            )))
        })
        .await;
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        assert_eq!(start.elapsed(), Duration::from_secs(4));
        for _ in 0..3 {
            assert!(matches!(
                rx.try_recv(),
                Ok(Input::Event(Event::TvPower { .. }))
            ));
        }
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn window_caps_queries_and_in_flight_requests() {
        let (tx, _rx) = mpsc::channel(8);
        let active = AtomicBool::new(true);
        let calls = AtomicUsize::new(0);
        let start = Instant::now();
        poll_window(&tx, &active, 2000, 5, || {
            calls.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Ok((
                Event::TvPower {
                    state: TvState::Off,
                    at: Utc::now(),
                },
                false,
            )))
        })
        .await;
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        assert_eq!(start.elapsed(), Duration::from_secs(5));
        let start = Instant::now();
        poll_window(&tx, &active, 2000, 5, || {
            std::future::pending::<anyhow::Result<(Event, bool)>>()
        })
        .await;
        assert_eq!(start.elapsed(), Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn shadow_and_permanent_errors_stop_without_observations() {
        let (tx, mut rx) = mpsc::channel(8);
        let active = AtomicBool::new(false);
        poll_window(&tx, &active, 2000, 45, || async {
            panic!("shadow must not query")
        })
        .await;
        active.store(true, Ordering::Release);
        let calls = AtomicUsize::new(0);
        poll_window(&tx, &active, 2000, 45, || {
            calls.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Err(webos::PairingRequired.into()))
        })
        .await;
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(rx.try_recv().is_err());
    }
}
