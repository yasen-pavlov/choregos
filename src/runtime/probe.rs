//! One read-only unicast console discovery request; silence is not standby.
use crate::state::ConsoleState;
use std::time::Duration;
use tokio::net::UdpSocket;

const SEARCH: &[u8] = b"SRCH * HTTP/1.1\ndevice-discovery-protocol-version:00030010\n";

fn parse(reply: &[u8]) -> Option<ConsoleState> {
    let line = std::str::from_utf8(reply.split(|byte| *byte == b'\n').next()?).ok()?;
    let mut parts = line.split_whitespace();
    if parts.next()? != "HTTP/1.1" {
        return None;
    }
    match parts.next()? {
        "200" => Some(ConsoleState::Awake),
        "620" => Some(ConsoleState::Standby),
        _ => None,
    }
}

pub async fn power_state(host: &str, port: u16) -> ConsoleState {
    let operation = async {
        let address = tokio::net::lookup_host((host, port))
            .await?
            .next()
            .ok_or_else(|| std::io::Error::other("probe host has no address"))?;
        let socket = UdpSocket::bind(if address.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })
        .await?;
        socket.connect(address).await?;
        socket.send(SEARCH).await?;
        let mut reply = [0; 2048];
        let length = socket.recv(&mut reply).await?;
        Ok::<_, std::io::Error>(parse(&reply[..length]).unwrap_or(ConsoleState::Unreachable))
    };
    match tokio::time::timeout(Duration::from_secs(1), operation).await {
        Ok(Ok(state)) => state,
        _ => ConsoleState::Unreachable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_first_response_line_determines_power() {
        assert_eq!(
            parse(b"HTTP/1.1 200 Ok\r\nhost-name:Test Console\n"),
            Some(ConsoleState::Awake)
        );
        assert_eq!(
            parse(b"HTTP/1.1 620 Server Standby\nhost-id:test\n"),
            Some(ConsoleState::Standby)
        );
        for invalid in [
            b"HTTP/1.1 2000 Bad\n".as_slice(),
            b"HTTP/1.0 200 Ok\n",
            b"garbage\nHTTP/1.1 200 Ok",
            b"HTTP/1.1 500 Error",
        ] {
            assert_eq!(parse(invalid), None);
        }
    }

    #[tokio::test]
    async fn udp_fake_receives_exact_search_and_returns_both_power_states() {
        for (reply, expected) in [
            ("HTTP/1.1 200 Ok", ConsoleState::Awake),
            ("HTTP/1.1 620 Server Standby", ConsoleState::Standby),
        ] {
            let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let port = server.local_addr().unwrap().port();
            let task = tokio::spawn(async move {
                let mut request = [0; 128];
                let (length, sender) = server.recv_from(&mut request).await.unwrap();
                assert_eq!(&request[..length], SEARCH);
                let reply = format!(
                    "{reply}\nhost-id:00000000\nhost-type:PS5\nhost-name:Test Console\nhost-request-port:997\nsystem-version:00000000\n"
                );
                server.send_to(reply.as_bytes(), sender).await.unwrap();
            });
            assert_eq!(power_state("127.0.0.1", port).await, expected);
            task.await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn silence_is_unreachable_after_one_second() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let start = tokio::time::Instant::now();
        assert_eq!(
            power_state("127.0.0.1", server.local_addr().unwrap().port()).await,
            ConsoleState::Unreachable
        );
        assert_eq!(start.elapsed(), Duration::from_secs(1));
    }
}
