use std::net::{Ipv4Addr, SocketAddrV4};
use tokio::net::UdpSocket;

pub fn magic_packet(mac: &str) -> anyhow::Result<[u8; 102]> {
    let bytes = crate::config::parse_mac(mac)?;
    let mut packet = [0xff; 102];
    for chunk in packet[6..].as_chunks_mut::<6>().0 {
        chunk.copy_from_slice(&bytes);
    }
    Ok(packet)
}
pub async fn wake(mac: &str, broadcast: Ipv4Addr, port: u16) -> anyhow::Result<()> {
    let packet = magic_packet(mac)?;
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    socket.set_broadcast(true)?;
    socket
        .send_to(&packet, SocketAddrV4::new(broadcast, port))
        .await?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packet_layout_and_bad_mac() {
        let packet = magic_packet("02:11:22:33:44:55").unwrap();
        assert_eq!(&packet[..6], &[255; 6]);
        assert!(
            packet[6..]
                .as_chunks::<6>()
                .0
                .iter()
                .all(|c| c == &[2, 17, 34, 51, 68, 85])
        );
        assert!(magic_packet("02:11").is_err());
    }
}
