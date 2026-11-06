//! Continuous direct power observation and action-scoped console windows.
use super::{Input, denon, probe, webos};
use crate::{
    config::{Config, ProbeConfig, ReceiverObserveConfig, TvOffConfig},
    engine::Event,
    state::{ConsoleState, DirectPower, Mode},
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
    receiver_task: Mutex<Option<JoinHandle<()>>>,
    consoles: Mutex<BTreeMap<String, (ConsoleState, JoinHandle<()>)>>,
}

impl Observer {
    pub fn new(tx: mpsc::Sender<Input>, active: Arc<AtomicBool>) -> Self {
        Self {
            tx,
            active,
            pairing_blocked: Arc::new(AtomicBool::new(false)),
            task: Mutex::new(None),
            receiver_task: Mutex::new(None),
            consoles: Mutex::new(BTreeMap::new()),
        }
    }

    /// Start continuous observation after adapters start (including reload).
    pub fn start(&self, config: Arc<Config>) {
        self.start_tv(config.clone(), false);
        self.receiver(config, false);
    }

    pub fn set_mode(&self, mode: Mode, config: &Config) {
        self.active.store(mode == Mode::Active, Ordering::Release);
        if mode != config.mode {
            self.stop();
            if mode == Mode::Active {
                let mut config = config.clone();
                config.mode = mode;
                self.start(Arc::new(config));
            }
        }
    }

    /// Called only after the ordered executor has dispatched the TV action.
    pub fn tv(&self, config: Arc<Config>) {
        self.start_tv(config, true);
    }

    fn start_tv(&self, config: Arc<Config>, window: bool) {
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
            poll_direct(
                &tx,
                &active,
                PollCadence::tv(&config.verify),
                window,
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
                            let _ = tx.send(Input::Event(Event::TvDirectUnavailable)).await;
                            return Err(error);
                        }
                        Err(_) => DirectPower::Unreachable,
                    };
                    Ok(Event::TvPower {
                        state,
                        at: Utc::now(),
                    })
                },
            )
            .await;
        });
    }

    fn replace(&self, operation: impl Future<Output = ()> + Send + 'static) {
        replace_task(&self.task, operation);
    }

    pub fn receiver(&self, config: Arc<Config>, window: bool) {
        if config.mode != Mode::Active || !self.active.load(Ordering::Acquire) {
            return;
        }
        let Some(ReceiverObserveConfig::Denon { host, port }) = &config.receiver.observe else {
            return;
        };
        let client = match denon::Client::new(host, *port) {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!(%error, "direct receiver observation could not start");
                return;
            }
        };
        let tx = self.tx.clone();
        let active = self.active.clone();
        replace_task(&self.receiver_task, async move {
            poll_direct(
                &tx,
                &active,
                PollCadence::receiver(&config.verify),
                window,
                || async {
                    let status = client.poll().await;
                    Ok(Event::ReceiverPower {
                        state: status.state,
                        at: Utc::now(),
                        source_code: status.source_code,
                    })
                },
            )
            .await;
        });
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
        for slot in [&self.task, &self.receiver_task] {
            if let Some(task) = slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                task.abort();
            }
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

fn replace_task(
    slot: &Mutex<Option<JoinHandle<()>>>,
    operation: impl Future<Output = ()> + Send + 'static,
) {
    let mut task = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(previous) = task.take() {
        previous.abort();
    }
    *task = Some(tokio::spawn(operation));
}

#[derive(Clone, Copy)]
struct PollCadence {
    poll_ms: u64,
    window_s: u64,
    idle_s: u64,
}

impl PollCadence {
    fn tv(verify: &crate::config::VerifyConfig) -> Self {
        Self {
            poll_ms: verify.tv_poll_ms,
            window_s: verify.tv_window_s,
            idle_s: verify.tv_idle_poll_s,
        }
    }
    fn receiver(verify: &crate::config::VerifyConfig) -> Self {
        Self {
            poll_ms: verify.receiver_poll_ms,
            window_s: verify.receiver_window_s,
            idle_s: verify.receiver_idle_poll_s,
        }
    }
}

/// Window completion returns to idle without spawning another task. Replacing
/// the owner's handle therefore cancels either cadence and any in-flight read.
async fn poll_direct<F, Fut>(
    tx: &mpsc::Sender<Input>,
    active: &AtomicBool,
    cadence: PollCadence,
    window: bool,
    mut query: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<Event>>,
{
    if window {
        let completed = poll_window(tx, active, cadence.poll_ms, cadence.window_s, || {
            let query = query();
            async move { query.await.map(|event| (event, false)) }
        })
        .await;
        if !completed {
            return;
        }
    }
    let period = Duration::from_secs(cadence.idle_s);
    let mut interval = tokio::time::interval_at(Instant::now() + period, period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    while active.load(Ordering::Acquire) && !tx.is_closed() {
        interval.tick().await;
        if !active.load(Ordering::Acquire) {
            break;
        }
        let Ok(event) = query().await else {
            break;
        };
        if !active.load(Ordering::Acquire) || tx.send(Input::Event(event)).await.is_err() {
            break;
        }
    }
}

/// Injectable query future keeps scheduling tests independent of TLS and hardware.
async fn poll_window<F, Fut>(
    tx: &mpsc::Sender<Input>,
    active: &AtomicBool,
    poll_ms: u64,
    window_s: u64,
    mut query: F,
) -> bool
where
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
        let (event, reached) = match tokio::time::timeout_at(deadline, query()).await {
            Ok(Ok(sample)) => sample,
            Ok(Err(_)) => return false,
            Err(_) => break,
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
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[tokio::test(start_paused = true)]
    async fn tv_idle_window_replacement_and_idle_resumption() {
        assert_idle_window_scheduling(false).await;
    }

    #[tokio::test(start_paused = true)]
    async fn receiver_idle_window_replacement_and_idle_resumption() {
        assert_idle_window_scheduling(true).await;
    }

    async fn assert_idle_window_scheduling(receiver: bool) {
        let (tx, mut rx) = mpsc::channel(16);
        let observer = Observer::new(tx.clone(), Arc::new(AtomicBool::new(true)));
        let verify = crate::config::VerifyConfig::default();
        let cadence = if receiver {
            PollCadence::receiver(&verify)
        } else {
            PollCadence::tv(&verify)
        };
        let slot = if receiver {
            &observer.receiver_task
        } else {
            &observer.task
        };
        let spawn_poll = |window| {
            let tx = tx.clone();
            let active = observer.active.clone();
            async move {
                poll_direct(&tx, &active, cadence, window, || {
                    std::future::ready(Ok(if receiver {
                        Event::ReceiverPower {
                            state: DirectPower::On,
                            at: Utc::now(),
                            source_code: Some("MPLAY".into()),
                        }
                    } else {
                        Event::TvPower {
                            state: DirectPower::On,
                            at: Utc::now(),
                        }
                    }))
                })
                .await;
            }
        };
        replace_task(slot, spawn_poll(false));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(14)).await;
        assert!(rx.try_recv().is_err());
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_ok());
        replace_task(slot, spawn_poll(true));
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_ok(), "window reads immediately");
        let window_ms = cadence.window_s * 1000;
        let polls = window_ms.div_ceil(cadence.poll_ms);
        for _ in 1..polls {
            tokio::time::advance(Duration::from_millis(cadence.poll_ms)).await;
            tokio::task::yield_now().await;
            assert!(rx.try_recv().is_ok(), "window keeps reading after target");
        }
        tokio::time::advance(Duration::from_millis(
            window_ms - (polls - 1) * cadence.poll_ms,
        ))
        .await;
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err(), "window ends at its full duration");
        tokio::time::advance(Duration::from_secs(cadence.idle_s - 1)).await;
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err());
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(
            rx.try_recv().is_ok(),
            "idle resumes after the window duration"
        );
        tokio::time::advance(Duration::from_secs(cadence.idle_s)).await;
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_ok(), "idle continues at the target");
        observer.stop();
        observer.active.store(false, Ordering::Release);
        replace_task(slot, spawn_poll(false));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(30)).await;
        assert!(rx.try_recv().is_err(), "shadow never samples");
    }

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
        let (socket, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut first_byte = [0; 1];
        tokio::time::timeout(Duration::from_secs(1), socket.peek(&mut first_byte))
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
    async fn pairing_prompt_emits_unavailable_and_blocks_later_action_queries() {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        use serde_json::json;
        // Disposable public loopback-only TLS identity, never a device credential.
        let certificate = hex::decode("3082017d30820123a00302010202141c37d2262d054dfcf76ecc0aa8b10d557ca2a10d300a06082a8648ce3d04030230143112301006035504030c096c6f63616c686f7374301e170d3236313130363039313430305a170d3336313130333039313430305a30143112301006035504030c096c6f63616c686f73743059301306072a8648ce3d020106082a8648ce3d03010703420004e89ea0130285a7d9a7fa02519327cedff13f859267c3d84b469d19aaae2ef6ad521178aaf0a915e63b83cf50597f1a6867f98abf7f00427f57cdddd269d1c853a3533051301d0603551d0e041604145f483546263760470239c00fbdb7170c840a8592301f0603551d230418301680145f483546263760470239c00fbdb7170c840a8592300f0603551d130101ff040530030101ff300a06082a8648ce3d0403020348003045022011d41c0aa2f6ecf0930afa5e963ef1c60f38b8ac902295e8c698032b708963e7022100b0eef97b2b01194ff5669dd3c72c07469df5f8f7f921d00e3c27863bda090cbe").unwrap();
        let key = hex::decode("308187020100301306072a8648ce3d020106082a8648ce3d030107046d306b020101042068de9b0435e4fdcec01fe24d4e3f9f5f1a7471e3116e1824f9f93deafef1cf73a14403420004e89ea0130285a7d9a7fa02519327cedff13f859267c3d84b469d19aaae2ef6ad521178aaf0a915e63b83cf50597f1a6867f98abf7f00427f57cdddd269d1c853").unwrap();
        let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(certificate)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = live_config(listener.local_addr().unwrap().port());
        let (tx, mut rx) = mpsc::channel(8);
        let observer = Observer::new(tx, Arc::new(AtomicBool::new(true)));
        observer.tv(config.clone());
        let socket = accept_in_flight_query(&listener).await;
        let server = tokio::task::spawn_blocking(move || {
            let socket = socket.into_std().unwrap();
            socket.set_nonblocking(false).unwrap();
            let session = rustls::ServerConnection::new(Arc::new(tls)).unwrap();
            let mut ws =
                tokio_tungstenite::tungstenite::accept(rustls::StreamOwned::new(session, socket))
                    .unwrap();
            let registration = ws.read().unwrap();
            let registration: serde_json::Value =
                serde_json::from_str(registration.to_text().unwrap()).unwrap();
            assert_eq!(registration["type"], "register");
            ws.send(tokio_tungstenite::tungstenite::Message::Text(
                json!({"type":"response","payload":{"pairingType":"PROMPT"}})
                    .to_string()
                    .into(),
            ))
            .unwrap();
        });
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap(),
            Some(Input::Event(Event::TvDirectUnavailable))
        ));
        server.await.unwrap();
        observer.tv(config);
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(180)).await;
        tokio::task::yield_now().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        assert!(rx.try_recv().is_err());
        observer.stop();
    }

    #[tokio::test]
    async fn newer_tv_action_replaces_the_in_flight_observation_window() {
        let old = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let new = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let observer = Observer::new(tx, Arc::new(AtomicBool::new(true)));
        observer.tv(live_config(old.local_addr().unwrap().port()));
        let first = accept_in_flight_query(&old).await;
        observer.tv(live_config(new.local_addr().unwrap().port()));
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
                state: DirectPower::Unreachable,
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
        observer.tv(live_config(listener.local_addr().unwrap().port()));
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
    async fn console_window_stops_on_target_at_poll_cadence() {
        let (tx, mut rx) = mpsc::channel(8);
        let active = AtomicBool::new(true);
        let calls = AtomicUsize::new(0);
        let start = Instant::now();
        poll_window(&tx, &active, 2000, 45, || {
            let call = calls.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Ok((
                Event::ConsolePower {
                    role: "streamer".into(),
                    state: if call == 2 {
                        ConsoleState::Standby
                    } else {
                        ConsoleState::Unreachable
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
                Ok(Input::Event(Event::ConsolePower { .. }))
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
                    state: DirectPower::Off,
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
    async fn permanent_query_errors_end_both_window_and_idle_without_resuming() {
        for window in [false, true] {
            let (tx, _rx) = mpsc::channel(8);
            let active = AtomicBool::new(true);
            let calls = AtomicUsize::new(0);
            let start = Instant::now();
            poll_direct(
                &tx,
                &active,
                PollCadence::tv(&crate::config::VerifyConfig::default()),
                window,
                || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    std::future::ready(Err(webos::PairingRequired.into()))
                },
            )
            .await;
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            assert_eq!(
                start.elapsed(),
                Duration::from_secs(if window { 0 } else { 15 })
            );
        }
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
