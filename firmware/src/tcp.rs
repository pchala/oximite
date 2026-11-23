//! Shared TCP socket shutdown handling.

use embassy_net::tcp::TcpSocket;
use embassy_time::{with_timeout, Duration};

pub async fn graceful_close(socket: &mut TcpSocket<'_>) {
    let _ = socket.flush().await;
    socket.close();
    let _ = with_timeout(Duration::from_millis(50), async {
        let mut trash = [0u8; 16];
        loop {
            if let Ok(0) | Err(_) = socket.read(&mut trash).await {
                break;
            }
        }
    })
    .await;
    socket.abort();
}
