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
  pub fn new(
    listen_addr: Option<Ipv4Addr>,
    listen_port: u16,
    shutdown: Receiver<()>,
  ) -> UdpSocketWrapper {
    UdpSocketWrapper {
      listen_addr: match listen_addr {
        Some(ip) => ip,
        None => Ipv4Addr::new(0, 0, 0, 0),
      },
      socket: None,
      listen_port,
      shutdown,
      dowork: true,
      recv_buff: [0; PACKET_BUFFER_SIZE],
    }
  }

  pub fn should_work(&self) -> bool {
    return self.dowork;
  }

  async fn ensure_we_have_socket(&mut self) {
    loop {
      if self.socket.is_some() {
        break;
      }
      select! {
        r = UdpSocket::bind(SocketAddr::new(std::net::IpAddr::V4(self.listen_addr), self.listen_port)) => {
          match r {
            Ok(s) => {
              self.socket = Some(s);
            },
            Err(e) => {
              error!("error binding to socket: {e:?}");
              sleep(Duration::from_secs(1)).await;
            }
          }
        },
        _ = self.shutdown.recv() => {
          self.socket = None;
          self.dowork = false;
          break;
        }
      };
    }
  }

  pub async fn recv(&mut self) -> Option<(SocketAddr, &[u8])> {
    loop {
      self.ensure_we_have_socket().await;
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
              sleep(Duration::from_secs(1)).await;
              // TODO maybe destroy socket?
              continue;
            }
          }
        },
        _ = self.shutdown.recv() => {
          self.dowork = false;
          return None;
        }
      };
    }
  }

  pub async fn send(&mut self, dst: &SocketAddr, packet: &[u8]) {
    self.ensure_we_have_socket().await;
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
