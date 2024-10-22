use log::{error, info};

use std::{
  net::{IpAddr, Ipv4Addr, SocketAddr},
  time::Duration,
};
use tokio::{net::UdpSocket, select, sync::broadcast::Receiver, time::sleep};

pub const MTU: usize = 1500;
const PACKET_BUFFER_SIZE: usize = MTU;

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
    let socket_opt = UdpSocket::bind(SocketAddr::new(std::net::IpAddr::V4(listen_addr), listen_port)).await;
    let socket = socket_opt.expect("error starting really needed listener");
    // TODO MAY PANIC: this error should be non-fatal because some apps may use Inferno as an optional audio I/O
    let listen_port = socket.local_addr().unwrap().port();
    UdpSocketWrapper {
      listen_addr,
      socket: Some(socket),
      listen_port,
      shutdown,
      dowork: true,
      recv_buff: [0; PACKET_BUFFER_SIZE],
    }
  }

  pub fn should_work(&self) -> bool {
    self.dowork
  }

  pub fn port(&self) -> u16 {
    self.listen_port
  }

  pub async fn recv(&mut self) -> Option<(SocketAddr, &[u8])> {
    let socket = match &self.socket {
      Some(s) => s,
      None => {
        return None;
      }
    };
    select! {
      r = socket.recv_from(&mut self.recv_buff) => {
        match r {
          Ok((len_recv, src)) => {
            return Some((src, &self.recv_buff[..len_recv]));
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

  pub async fn send(&mut self, dst: &SocketAddr, packet: &[u8]) {
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
