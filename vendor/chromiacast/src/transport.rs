use std::future::Future;
use std::io;
use std::net::SocketAddr;
use tokio::net::UdpSocket;

/// Abstraction over the network transport for sending and receiving packets.
///
/// The default implementation wraps `tokio::net::UdpSocket`. Custom
/// implementations enable testing with mock transports or tunneling
/// through alternative channels.
pub trait Transport: Send + Sync + 'static {
    fn send_to(
        &self,
        data: &[u8],
        address: SocketAddr,
    ) -> impl Future<Output = io::Result<usize>> + Send;

    fn recv_from(
        &self,
        buf: &mut [u8],
    ) -> impl Future<Output = io::Result<(usize, SocketAddr)>> + Send;

    fn local_addr(&self) -> io::Result<SocketAddr>;
}

/// Default UDP transport wrapping a `tokio::net::UdpSocket`.
pub struct UdpTransport {
    socket: UdpSocket,
}

impl UdpTransport {
    pub async fn bind(address: SocketAddr) -> io::Result<Self> {
        let socket = UdpSocket::bind(address).await?;
        Ok(Self { socket })
    }

    pub fn from_socket(socket: UdpSocket) -> Self {
        Self { socket }
    }
}

impl Transport for UdpTransport {
    async fn send_to(&self, data: &[u8], address: SocketAddr) -> io::Result<usize> {
        self.socket.send_to(data, address).await
    }

    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.socket.recv_from(buf).await
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}
