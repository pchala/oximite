//! Minimal DHCP server, only used in soft-AP setup mode.
//!
//! Hands every client the same fixed lease (192.168.4.2) with the board as
//! gateway; there is no lease table because the AP exists purely so one phone
//! can reach the setup page.

use embassy_net::udp::{PacketMetadata, UdpSocket};

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
        if let Ok((n, _remote)) = socket.recv_from(&mut buf).await {
            if n < 240 {
                continue;
            }
            if buf[236..240] != [0x63, 0x82, 0x53, 0x63] {
                continue;
            }

            let xid = &buf[4..8];
            let chaddr = &buf[28..44];
            let mut msg_type = 0;

            let mut opt_ptr = 240;
            while opt_ptr < n {
                let code = buf[opt_ptr];
                if code == 255 {
                    break;
                }
                if code == 0 {
                    opt_ptr += 1;
                    continue;
                }
                if opt_ptr + 1 >= n {
                    break;
                }
                let len = buf[opt_ptr + 1] as usize;
                if code == 53 && len == 1 && opt_ptr + 2 < n {
                    msg_type = buf[opt_ptr + 2];
                }
                opt_ptr += 2 + len;
            }

            if msg_type == 1 || msg_type == 3 {
                let mut reply = [0u8; 300];
                reply[0] = 2;
                reply[1] = 1;
                reply[2] = 6;
                reply[4..8].copy_from_slice(xid);
                reply[16..20].copy_from_slice(&[192, 168, 4, 2]);
                reply[20..24].copy_from_slice(&[192, 168, 4, 1]);
                reply[28..44].copy_from_slice(chaddr);
                reply[236..240].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]);

                let next_type = if msg_type == 1 { 2 } else { 5 };
                reply[240..243].copy_from_slice(&[53, 1, next_type]);
                reply[243..249].copy_from_slice(&[54, 4, 192, 168, 4, 1]);
                reply[249..255].copy_from_slice(&[51, 4, 0, 0, 14, 16]);
                reply[255..261].copy_from_slice(&[1, 4, 255, 255, 255, 0]);
                reply[261..267].copy_from_slice(&[3, 4, 192, 168, 4, 1]);
                reply[267] = 255;

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
}
