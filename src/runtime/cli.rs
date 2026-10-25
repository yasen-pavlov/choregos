use crate::{
    cec::Frame,
    config::{self, Config},
    engine::{Engine, Event},
};
use std::{
    io::Write,
    path::Path,
    time::{Duration, Instant},
};

pub async fn run(path: std::path::PathBuf) -> anyhow::Result<()> {
    let (config, text) = config::load(&path)?;
    super::run(config, text, path).await
}
pub fn check_config(path: &Path) -> anyhow::Result<String> {
    let (config, _) = config::load(path)?;
    Ok(config.redacted_text()?)
}
pub async fn healthz(url: &str) -> anyhow::Result<()> {
    let response = openidconnect::reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?
        .get(url)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("health endpoint unavailable"))?;
    anyhow::ensure!(
        response.status().is_success(),
        "health endpoint returned an error"
    );
    let health: serde_json::Value = serde_json::from_slice(&response.bytes().await?)?;
    anyhow::ensure!(health.get("mode").is_some(), "invalid health response");
    Ok(())
}
pub fn replay_file(config: &Path, fixture: &Path, output: &mut impl Write) -> anyhow::Result<()> {
    replay(
        Config::parse(&std::fs::read_to_string(config)?)?,
        &std::fs::read_to_string(fixture)?,
        output,
    )
}
pub fn replay(config: Config, text: &str, output: &mut impl Write) -> anyhow::Result<()> {
    let start = Instant::now();
    let mut engine = Engine::new(config, start);
    let mut epoch = None;
    let mut last = None;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let value: serde_json::Value = serde_json::from_str(line)?;
        let timestamp = chrono::DateTime::parse_from_rfc3339(
            value["ts"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("fixture missing timestamp"))?,
        )?
        .with_timezone(&chrono::Utc);
        let epoch = *epoch.get_or_insert(timestamp);
        let now = start + (timestamp - epoch).to_std()?;
        while let Some(deadline) = engine.next_deadline().filter(|d| *d < now) {
            let wall = epoch + chrono::Duration::from_std(deadline.duration_since(start))?;
            engine.handle(Event::Tick, deadline, wall);
            for decision in &engine.decisions {
                writeln!(output, "{}", serde_json::to_string(decision)?)?;
            }
        }
        let event = if value["topic"].as_str().is_some_and(|s| s.ends_with("/LWT")) {
            match value["payload"].as_str() {
                Some("Online") => Some(Event::BridgeOnline),
                Some("Offline") => Some(Event::BridgeOffline),
                _ => None,
            }
        } else {
            Frame::parse(&serde_json::to_vec(&value["payload"])?)
                .ok()
                .map(Event::Frame)
        };
        if let Some(event) = event {
            engine.handle(event, now, timestamp);
            for decision in &engine.decisions {
                writeln!(output, "{}", serde_json::to_string(decision)?)?;
            }
        }
        last = Some((now, timestamp));
    }
    if let Some((now, wall)) = last {
        engine.handle(
            Event::Tick,
            now + Duration::from_secs(5),
            wall + chrono::Duration::seconds(5),
        );
        for decision in &engine.decisions {
            writeln!(output, "{}", serde_json::to_string(decision)?)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replay_outputs_actual_decisions_without_environment_or_network() {
        let config = Config::parse(include_str!("../../choregos.example.toml")).unwrap();
        let mut output = vec![];
        replay(
            config,
            include_str!("../../fixtures/capture-streamer-wake.jsonl"),
            &mut output,
        )
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("takeover_settled"));
        assert!(text.contains("off_timer_expired"));
        assert!(
            text.lines()
                .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok())
        );
    }
    #[tokio::test]
    async fn health_probe_accepts_health_json_and_rejects_invalid_responses() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = axum::Router::new()
            .route(
                "/good",
                axum::routing::get(|| async { axum::Json(serde_json::json!({"mode":"shadow"})) }),
            )
            .route("/bad", axum::routing::get(|| async { "{}" }));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        healthz(&format!("http://{address}/good")).await.unwrap();
        assert!(healthz(&format!("http://{address}/bad")).await.is_err());
        assert!(healthz(&format!("http://{address}/missing")).await.is_err());
        server.abort();
    }
}
