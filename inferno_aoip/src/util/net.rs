use log::{error, info};

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio::{net::UdpSocket, select, sync::broadcast::Receiver};

pub const MTU: usize = 1500;
const PACKET_BUFFER_SIZE: usize = MTU;
pub const MAX_PAYLOAD_BYTES: usize = 1400; // ???

pub struct ReceiveBuffer {
  buff: [u8; PACKET_BUFFER_SIZE],
}

impl ReceiveBuffer {
  pub fn new() -> Self {
    Self { buff: [0u8; PACKET_BUFFER_SIZE] }
  }
}

pub struct UdpSocketWrapper {
  socket: Option<UdpSocket>,
  listen_addr: Ipv4Addr,
  listen_port: u16,
  shutdown: Receiver<()>,
  dowork: bool,
  recv_buff: [u8; PACKET_BUFFER_SIZE],
}

impl UdpSocketWrapper {
  pub async fn new(
    listen_addr: Option<Ipv4Addr>,
    listen_port: u16,
    shutdown: Receiver<()>,
  ) -> UdpSocketWrapper {
    let listen_addr = listen_addr.unwrap_or(Ipv4Addr::new(0, 0, 0, 0));
    let socket_opt =
      UdpSocket::bind(SocketAddr::new(std::net::IpAddr::V4(listen_addr), listen_port)).await;
    match socket_opt {
      Ok(socket) => {
        let listen_port = socket.local_addr().map(|a| a.port()).unwrap_or(listen_port);
        UdpSocketWrapper {
          listen_addr,
          socket: Some(socket),
          listen_port,
          shutdown,
          dowork: true,
          recv_buff: [0; PACKET_BUFFER_SIZE],
        }
      }
      Err(e) => {
        error!("error starting listener on {listen_addr}:{listen_port}: {e}");
        UdpSocketWrapper {
          listen_addr,
          socket: None,
          listen_port,
          shutdown,
          dowork: false,
          recv_buff: [0; PACKET_BUFFER_SIZE],
        }
      }
    }
  }

  pub fn should_work(&self) -> bool {
    self.dowork
  }

  pub fn port(&self) -> u16 {
    self.listen_port
  }

  pub async fn recv<'a>(&mut self, recv_buff: &'a mut ReceiveBuffer) -> Option<(SocketAddr, &'a [u8])> {
    let socket = match &self.socket {
      Some(s) => s,
      None => {
        return None;
      }
    };
    select! {
      r = socket.recv_from(&mut recv_buff.buff) => {
        match r {
          Ok((len_recv, src)) => {
            return Some((src, &recv_buff.buff[..len_recv]));
          },
          Err(e) => {
            error!("error receiving from socket: {e:?}");
            return None;
          }
        }
      },
      _ = self.shutdown.recv() => {
        self.dowork = false;
        return None;
      }
    };
  }

  pub async fn send(&self, dst: &SocketAddr, packet: &[u8]) {
    let socket = match &self.socket {
      Some(s) => s,
      None => {
        info!("shutting down, discarding message to send");
        return;
      }
    };
    if let Err(e) = socket.send_to(packet, dst).await {
      error!("send error (ignoring): {e:?}");
    }
  }
}

pub async fn create_tokio_udp_socket(self_ip: Ipv4Addr) -> tokio::io::Result<(UdpSocket, u16)> {
  let socket = UdpSocket::bind(SocketAddr::new(IpAddr::V4(self_ip), 0)).await?;
  let port = socket.local_addr()?.port();
  return Ok((socket, port));
}

pub fn create_mio_udp_socket(self_ip: Ipv4Addr) -> std::io::Result<(mio::net::UdpSocket, u16)> {
  let socket = mio::net::UdpSocket::bind(SocketAddr::new(IpAddr::V4(self_ip), 0))?;
  let port = socket.local_addr()?.port();
  return Ok((socket, port));
}
