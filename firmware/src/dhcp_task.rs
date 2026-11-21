//! Minimal DHCP UDP server task for AP setup mode.

use embassy_net::udp::{PacketMetadata, UdpSocket};

use crate::dhcp_codec;

#[embassy_executor::task]
pub async fn dhcp_server_task(stack: &'static embassy_net::Stack<'static>) {
    let mut rx_meta = [PacketMetadata::EMPTY; 2];
    let mut rx_buffer = [0u8; 1024];
    let mut tx_meta = [PacketMetadata::EMPTY; 2];
    let mut tx_buffer = [0u8; 1024];
    let mut buf = [0u8; 1024];

    let mut socket = UdpSocket::new(
        *stack,
        &mut rx_meta,
        &mut rx_buffer,
        &mut tx_meta,
        &mut tx_buffer,
    );
    let _ = socket.bind(67);

    loop {
        let Ok((n, _remote)) = socket.recv_from(&mut buf).await else {
            continue;
        };
        let mut reply = [0u8; 300];
        if dhcp_codec::build_reply(&buf[..n], &mut reply) {
            let _ = socket
                .send_to(
                    &reply,
                    embassy_net::IpEndpoint::new(
                        embassy_net::IpAddress::v4(255, 255, 255, 255),
                        68,
                    ),
                )
                .await;
        }
    }
}
