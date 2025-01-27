// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use futures::{
  future::{BoxFuture, FutureExt},
  AsyncReadExt,
};
use std::{
  fmt::Display,
  net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
  str::FromStr,
  sync::Weak,
};
use tokio::{
  io::{AsyncRead, AsyncWrite},
  net::{TcpStream, ToSocketAddrs},
};
use tracing_futures::Instrument;

use super::{
  tunnel::{Tunnel, TunnelId},
  Client, ClientError, DynamicResponseClient, Request, Response, RouteAddress, Router,
  RoutingError, Service, ServiceError,
};
use crate::util::{proxy_generic_tokio_streams, tunnel_stream::TunnelStream};

#[derive(Debug, Clone)]
pub struct TcpStreamClient<Reader, Writer> {
  recv: Reader,
  send: Writer,
}

impl<Reader, Writer> TcpStreamClient<Reader, Writer> {
  pub fn new(recv: Reader, send: Writer) -> Self {
    Self { recv, send }
  }

  pub fn build_addr(target: TcpStreamTarget) -> RouteAddress {
    target.to_string()
  }
}

impl<Reader, Writer> Client for TcpStreamClient<Reader, Writer>
where
  Reader: AsyncRead + Send + Unpin + 'static,
  Writer: AsyncWrite + Send + Unpin + 'static,
{
  // TODO: make Response the number of bytes forwarded by the client
  type Response = ();

  fn handle(
    mut self,
    _addr: RouteAddress,
    tunnel: Box<dyn TunnelStream + Send + 'static>,
  ) -> BoxFuture<Result<Self::Response, ClientError>> {
    let fut = async move {
      // TODO: Read protocol version here, and ServiceError::Refused if unsupported
      // TODO: Send protocol version here, allow other side to refuse if unsupported
      // If a confirmation of support is received by the reading side, resume as supported version
      let (mut tunr, mut tunw) = tokio::io::split(tunnel);
      proxy_generic_tokio_streams((&mut self.send, &mut self.recv), (&mut tunw, &mut tunr)).await;
      tracing::info!(target = "proxy_tcp_close", "Closing stream");
      Ok(())
    };
    fut.fuse().boxed()
  }
}

#[derive(Debug)]
pub struct TcpStreamService {
  pub local_only: bool,
}

#[derive(Debug, Copy, Clone)]
enum TcpConnectError {
  ConnectionFailed,
  NoLoopbackAddressesFound,
}

#[derive(thiserror::Error, Debug)]
pub enum TargetResolutionError {
  #[error("DNS resolution failure")]
  IOError(#[from] std::io::Error, std::backtrace::Backtrace),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DnsTarget {
  PreferHigher { host: String, port: u16 },
  Dns4 { host: String, port: u16 },
  Dns6 { host: String, port: u16 },
}

impl DnsTarget {
  pub fn includes_ipv6(&self) -> bool {
    match self {
      DnsTarget::PreferHigher { .. } => true,
      DnsTarget::Dns6 { .. } => true,
      DnsTarget::Dns4 { .. } => false,
    }
  }

  pub fn includes_ipv4(&self) -> bool {
    match self {
      DnsTarget::PreferHigher { .. } => true,
      DnsTarget::Dns6 { .. } => false,
      DnsTarget::Dns4 { .. } => true,
    }
  }

  /// Exposed port for the specified address
  ///
  /// Not all DNS records have a constant, known port; See [SRV records](https://en.wikipedia.org/wiki/SRV_record)
  pub fn port(&self) -> Option<u16> {
    match self {
      DnsTarget::PreferHigher { port, .. } => Some(*port),
      DnsTarget::Dns6 { port, .. } => Some(*port),
      DnsTarget::Dns4 { port, .. } => Some(*port),
    }
  }

  /// Checks that a [SocketAddr] is valid in the range of the specified DNS class
  pub fn contains(&self, addr: &SocketAddr, check_port: bool) -> bool {
    if check_port && Some(addr.port()) != self.port() {
      false
    } else {
      addr.is_ipv6() && self.includes_ipv6() || addr.is_ipv4() && self.includes_ipv4()
    }
  }
}

impl Into<TcpStreamTarget> for DnsTarget {
  fn into(self) -> TcpStreamTarget {
    TcpStreamTarget::Dns(self)
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TcpStreamTarget {
  Port(u16),
  SocketAddr(SocketAddr),
  Dns(DnsTarget),
}

/// Format a [RouteAddress] from a [TcpStreamTarget]
impl Display for TcpStreamTarget {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    use std::net::{SocketAddrV4, SocketAddrV6};
    match self {
      TcpStreamTarget::Port(port) => write!(f, "/tcp/{}", port),
      TcpStreamTarget::SocketAddr(SocketAddr::V4(s)) => {
        write!(f, "/ip4/{}/tcp/{}", s.ip(), s.port())
      }
      TcpStreamTarget::SocketAddr(SocketAddr::V6(s)) => {
        write!(f, "/ip6/{}/tcp/{}", s.ip(), s.port())
      }
      TcpStreamTarget::Dns(DnsTarget::PreferHigher { host, port }) => {
        write!(f, "/dns/{}/tcp/{}", host, port)
      }
      TcpStreamTarget::Dns(DnsTarget::Dns4 { host, port }) => {
        write!(f, "/dns4/{}/tcp/{}", host, port)
      }
      TcpStreamTarget::Dns(DnsTarget::Dns6 { host, port }) => {
        write!(f, "/dns6/{}/tcp/{}", host, port)
      }
    }
  }
}

#[derive(thiserror::Error, Debug)]
pub enum TcpStreamTargetParseError {
  #[error("Not enough segments present to represent valid target")]
  TooFewSegments,
  #[error("Addresses must start with a '/' character")]
  InvalidPrefix,
  #[error("No supported address type matches the provided format")]
  NoMatchingFormat,
  #[error("Port specification invalid")]
  InvalidPort(#[from] std::num::ParseIntError, std::backtrace::Backtrace),
  #[error("IP format invalid")]
  InvalidIP(#[from] std::net::AddrParseError, std::backtrace::Backtrace),
}

/// Try to parse a [RouteAddress] into a [TcpStreamTarget]
///
/// Expects /tcp/<port>, /ip[46]/address/tcp/port, or /dns[46]?/address/tcp/port
///
/// DNS resolution is not handled here, only parsed to its own class for use later.
///
/// /tcp/<port> directs to localhost with an IPv6 preference, and is equivalent to
/// /dns/localhost/tcp/<port> but skips the DNS resolver and ignores the hostfile.
// TODO: hostname validation; use a dedicated DNS library and fail invalid names.
// TODO: Use a recursive descent parsing combinator library such as Nom
impl FromStr for TcpStreamTarget {
  type Err = TcpStreamTargetParseError;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    let parts = s.splitn(5, '/').collect::<Vec<_>>();
    let (prefix, parts) = parts
      .split_first()
      .ok_or(TcpStreamTargetParseError::TooFewSegments)?;
    if !prefix.is_empty() {
      return Err(TcpStreamTargetParseError::InvalidPrefix);
    }
    let (port, parts) = parts
      .split_last()
      .ok_or(TcpStreamTargetParseError::TooFewSegments)?;
    let port: u16 = port.parse()?;
    match parts {
      ["tcp"] => Ok(TcpStreamTarget::SocketAddr(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        port,
      ))),
      ["ip4", addr, "tcp"] => addr
        .parse::<Ipv4Addr>()
        .map_err(Into::into)
        .map(|addr| TcpStreamTarget::SocketAddr(SocketAddr::new(IpAddr::V4(addr), port))),
      ["ip6", addr, "tcp"] => addr
        .parse::<Ipv6Addr>()
        .map_err(Into::into)
        .map(|addr| TcpStreamTarget::SocketAddr(SocketAddr::new(IpAddr::V6(addr), port))),
      [dns_class @ ("dns" | "dns4" | "dns6"), host, "tcp"] => {
        let host = host.to_string();
        Ok(TcpStreamTarget::Dns(match *dns_class {
          "dns" => DnsTarget::PreferHigher { host, port },
          "dns6" => DnsTarget::Dns6 { host, port },
          "dns4" => DnsTarget::Dns4 { host, port },
          _ => unreachable!("Checked statically via matcher"),
        }))
      }
      _ => Err(TcpStreamTargetParseError::NoMatchingFormat),
    }
  }
}

impl TcpStreamService {
  pub fn new(local_only: bool) -> Self {
    Self { local_only }
  }

  /// The `connect` future outlives the read reference lifetime to `self`
  /// This allows us to capture configuration such as `local_only` before
  /// producing a generator; that generator can then refuse to serve when
  /// it is processed.
  fn connect(
    &'_ self,
    mut addrs: Vec<SocketAddr>,
  ) -> BoxFuture<'_, Result<Result<TcpStream, std::io::Error>, TcpConnectError>> {
    let local_only = self.local_only;
    let fut = async move {
      if addrs.is_empty() {
        return Err(TcpConnectError::ConnectionFailed);
      }
      if local_only {
        addrs.drain_filter(|x| !x.ip().is_loopback()).last();
        if addrs.is_empty() {
          return Err(TcpConnectError::NoLoopbackAddressesFound);
        }
      }
      Ok(TcpStream::connect(addrs.as_slice()).await.and_then(|c| {
        c.set_nodelay(true)?;
        Ok(c)
      }))
    };
    fut.fuse().boxed()
  }

  async fn resolve_dns(&self, target: DnsTarget) -> Result<Vec<SocketAddr>, TargetResolutionError> {
    // TODO: use a purpose-built library for DNS resolution
    use tokio::net::lookup_host;
    let resolved = lookup_host(match &target {
      DnsTarget::PreferHigher { host, port }
      | DnsTarget::Dns6 { host, port }
      | DnsTarget::Dns4 { host, port } => {
        format!("{}:{}", host, port)
      }
    })
    .await?;
    let matching_scheme = resolved.filter(|addr| target.contains(addr, true));
    Ok(matching_scheme.collect())
  }

  async fn resolve(
    &self,
    target: TcpStreamTarget,
  ) -> Result<Vec<SocketAddr>, TargetResolutionError> {
    match target {
      TcpStreamTarget::Port(port) => Ok(
        [
          SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port),
          SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
        ]
        .to_vec(),
      ),
      TcpStreamTarget::SocketAddr(s) => Ok([s].to_vec()),
      TcpStreamTarget::Dns(dns_target) => self.resolve_dns(dns_target).await,
    }
  }
}

impl Service for TcpStreamService {
  fn accepts(&self, addr: &RouteAddress, _tunnel_id: &TunnelId) -> bool {
    addr.parse::<TcpStreamTarget>().is_ok()
  }

  fn handle<'a>(
    &'a self,
    addr: RouteAddress,
    stream: Box<dyn TunnelStream + Send + 'static>,
    _tunnel_id: TunnelId,
  ) -> BoxFuture<'a, Result<(), ServiceError>> {
    use futures::future::Either;
    tracing::debug!(
      "TCP proxy connection received for {}; building span...",
      addr
    );
    let span = tracing::span!(tracing::Level::DEBUG, "proxy_tcp", target = ?addr);
    let target = match addr
      .parse::<TcpStreamTarget>()
      .map_err(|_| ServiceError::AddressError)
    {
      Err(e) => return futures::future::ready(Err(e)).boxed(),
      Ok(target) => target,
    };
    let fut = async move {
      // TODO: Read protocol version here, and ServiceError::Refused if unsupported
      // TODO: Send protocol version here, allow other side to refuse if unsupported
      // If a confirmation of support is received by the reading side, resume as supported version
      let addrs = self
        .resolve(target)
        .await
        .or(Err(ServiceError::AddressError))?;
      let connector = self.connect(addrs);
      tracing::debug!(
        target = "proxy_tcp_connecting",
        "Connecting to proxy destination"
      );
      let mut connection: TcpStream = connector
        .await
        .map_err(|e| match e {
          TcpConnectError::ConnectionFailed => ServiceError::DependencyFailure,
          TcpConnectError::NoLoopbackAddressesFound => ServiceError::AddressError,
        })?
        .map_err(|_| ServiceError::DependencyFailure)?;
      tracing::debug!(target = "proxy_tcp_streaming", "Performing proxy streaming");

      let (mut tcpr, mut tcpw) = connection.split();
      let (mut tunr, mut tunw) = tokio::io::split(stream);

      proxy_generic_tokio_streams((&mut tcpw, &mut tcpr), (&mut tunw, &mut tunr)).await;
      tracing::info!(target = "proxy_tcp_close", "Closing stream");
      Ok(())
    };

    fut.instrument(span).boxed()
  }
}
