// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license OR Apache 2.0
use crate::util::tunnel_stream::{TunnelStream, WrappedStream};
use downcast_rs::{impl_downcast, Downcast, DowncastSync};
use futures::future::{BoxFuture, FutureExt};
use std::{
  any::Any,
  backtrace::Backtrace,
  collections::BTreeMap,
  fmt::Debug,
  sync::{Arc, Weak},
};

use super::tunnel::{Tunnel, TunnelId, TunnelName};
use crate::common::protocol::tunnel::TunnelError;

pub type RouteAddress = String;

pub struct Request {
  pub address: RouteAddress,
  pub protocol_client: Box<dyn DynamicResponseClient + Send + Sync + 'static>,
}

impl Debug for Request {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Request")
      .field("address", &self.address)
      .finish_non_exhaustive()
  }
}

pub struct Response {
  content: Box<dyn Any>,
}

impl Response {
  pub fn new(content: Box<dyn Any>) -> Self {
    Self { content }
  }
  pub fn content(&self) -> &Box<dyn Any> {
    &self.content
  }
  pub fn into_inner(self) -> Box<dyn Any> {
    self.content
  }
}

impl Request {
  pub fn new<TProtocolClient>(address: RouteAddress, protocol_client: TProtocolClient) -> Self
  where
    TProtocolClient: Client + Send + Sync + 'static,
  {
    Self {
      address,
      protocol_client: Box::new(protocol_client),
    }
  }
}

#[derive(Clone)]
pub struct TunnelRecord {
  pub id: TunnelId,
  pub name: Option<TunnelName>,
  pub tunnel: Arc<dyn Tunnel + Send + Sync + Unpin + 'static>,
}

impl Debug for TunnelRecord {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct(stringify!(TunnelRecord))
      .field("id", &self.id)
      .field("name", &self.name)
      .finish_non_exhaustive()
  }
}

#[derive(thiserror::Error, Debug)]
pub enum TunnelRegistrationError {
  #[error("Tunnel ID was already occupied")]
  IdOccupied(TunnelId),
  #[error("Tunnel name was already occupied and auto-replacement was refused")]
  NameOccupied(TunnelName),
  #[error(transparent)]
  ApplicationError(anyhow::Error),
}

#[derive(thiserror::Error, Debug)]
pub enum TunnelNamingError {
  #[error("Tunnel name was already occupied and auto-replacement was refused")]
  NameOccupied(TunnelName),
  #[error("The tunnel to be named was not found")]
  TunnelNotRegistered(TunnelId),
  #[error(transparent)]
  ApplicationError(anyhow::Error),
}

pub trait TunnelRegistry: Downcast + DowncastSync {
  fn lookup_by_id(&self, tunnel_id: TunnelId) -> BoxFuture<Option<TunnelRecord>>;
  fn lookup_by_name(&self, tunnel_name: TunnelName) -> BoxFuture<Option<TunnelRecord>>;

  /// Called prior to authentication, a tunnel is not yet trusted and has no name,
  /// but the ID is guaranteed to remain stable throughout its lifetime.
  ///
  /// Upon disconnection, [Self::drop_tunnel] will be called with the given [TunnelId].
  fn register_tunnel(
    &self,
    tunnel_id: TunnelId,
    tunnel: Arc<dyn Tunnel + Send + Sync + Unpin + 'static>,
  ) -> BoxFuture<Result<(), TunnelRegistrationError>>;

  /// Called after authentication, when a tunnel is given an official designation
  /// May also be called later to allow a reconnecting tunnel to replaace its old
  /// record until that record is removed.
  fn name_tunnel(
    &self,
    tunnel_id: TunnelId,
    name: TunnelName,
  ) -> BoxFuture<Result<(), TunnelNamingError>>;

  /// Called to remove a tunnel from the registry after it is disconnected.
  /// Does not immediately destroy the Tunnel; previous consumers can hold
  /// an Arc containing the Tunnel instance, which will extend its lifetime.
  fn deregister_tunnel(&self, tunnel_id: TunnelId) -> BoxFuture<Result<TunnelRecord, ()>>;
}
impl_downcast!(sync TunnelRegistry);

impl<T> TunnelRegistry for Arc<T>
where
  T: TunnelRegistry + Send + Sync + 'static,
{
  fn lookup_by_id(&self, tunnel_id: TunnelId) -> BoxFuture<'_, Option<TunnelRecord>> {
    self.as_ref().lookup_by_id(tunnel_id)
  }

  fn lookup_by_name(&self, tunnel_name: TunnelName) -> BoxFuture<'_, Option<TunnelRecord>> {
    self.as_ref().lookup_by_name(tunnel_name)
  }

  fn register_tunnel(
    &self,
    tunnel_id: TunnelId,
    tunnel: Arc<dyn Tunnel + Send + Sync + Unpin>,
  ) -> BoxFuture<'_, Result<(), TunnelRegistrationError>> {
    self.as_ref().register_tunnel(tunnel_id, tunnel)
  }

  fn name_tunnel(
    &self,
    tunnel_id: TunnelId,
    name: TunnelName,
  ) -> BoxFuture<'_, Result<(), TunnelNamingError>> {
    self.as_ref().name_tunnel(tunnel_id, name)
  }

  fn deregister_tunnel(&self, tunnel_id: TunnelId) -> BoxFuture<'_, Result<TunnelRecord, ()>> {
    self.as_ref().deregister_tunnel(tunnel_id)
  }
}

pub struct InMemoryTunnelRegistry {
  tunnels: Arc<tokio::sync::Mutex<BTreeMap<TunnelId, TunnelRecord>>>,
}

impl InMemoryTunnelRegistry {
  pub fn new() -> Self {
    Self {
      tunnels: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
    }
  }

  pub async fn keys(&self) -> Vec<TunnelId> {
    let lock = self.tunnels.lock().await;
    lock.keys().cloned().collect()
  }

  pub async fn max_key(&self) -> Option<TunnelId> {
    let lock = self.tunnels.lock().await;
    lock.keys().max().cloned()
  }
}

impl TunnelRegistry for InMemoryTunnelRegistry {
  fn lookup_by_id(&self, tunnel_id: TunnelId) -> BoxFuture<Option<TunnelRecord>> {
    let tunnels = Arc::clone(&self.tunnels);
    async move {
      let tunnels = tunnels.lock().await;
      let tunnel = tunnels.get(&tunnel_id);
      tunnel.cloned()
    }
    .boxed()
  }

  fn lookup_by_name(&self, tunnel_name: TunnelName) -> BoxFuture<Option<TunnelRecord>> {
    let tunnels = Arc::clone(&self.tunnels);
    async move {
      let tunnels = tunnels.lock().await;
      // Note: Inefficient total enumeration, replace with hash lookup
      let tunnel = tunnels
        .iter()
        .find(|(_id, record)| record.name.as_ref() == Some(&tunnel_name))
        .map(|(_id, record)| record.clone());
      tunnel
    }
    .boxed()
  }

  fn register_tunnel(
    &self,
    tunnel_id: TunnelId,
    tunnel: Arc<dyn Tunnel + Send + Sync + Unpin + 'static>,
  ) -> BoxFuture<Result<(), TunnelRegistrationError>> {
    let tunnels = Arc::clone(&self.tunnels);
    async move {
      let mut tunnels = tunnels.lock().await;
      if tunnels.contains_key(&tunnel_id) {
        return Err(TunnelRegistrationError::IdOccupied(tunnel_id));
      }
      assert!(
        tunnels
          .insert(
            tunnel_id,
            TunnelRecord {
              id: tunnel_id,
              name: None,
              tunnel,
            },
          )
          .is_none(),
        "TunnelId overlap despite locked map where contains_key returned false"
      );
      Ok(())
    }
    .boxed()
  }

  fn name_tunnel(
    &self,
    tunnel_id: TunnelId,
    name: TunnelName,
  ) -> BoxFuture<Result<(), TunnelNamingError>> {
    let tunnels = Arc::clone(&self.tunnels);
    async move {
      let tunnels = tunnels.lock().await;
      {
        let tunnel = match tunnels.get(&tunnel_id) {
          // Event may have been processed after the tunnel
          // was deregistered, or before it was registered.
          None => return Err(TunnelNamingError::TunnelNotRegistered(tunnel_id)),
          Some(t) => t,
        };

        // If any tunnel other than this one currently has the given name, bail
        // Note: Inefficient total enumeration, replace with hash lookup
        if tunnels
          .iter()
          .any(|(id, record)| record.name.as_ref() == Some(&name) && id != &tunnel.id)
        {
          return Err(TunnelNamingError::NameOccupied(name));
        }
      }

      let mut tunnels = tunnels;
      tunnels.get_mut(&tunnel_id);
      let tunnel = tunnels
        .get_mut(&tunnel_id)
        .expect("We were just holding this, and still have the lock");

      tunnel.name = Some(name);

      Ok(())
    }
    .boxed()
  }

  fn deregister_tunnel(&self, tunnel_id: TunnelId) -> BoxFuture<Result<TunnelRecord, ()>> {
    let tunnels = Arc::clone(&self.tunnels);
    async move {
      let mut tunnels = tunnels.lock().await;
      tunnels.remove(&tunnel_id).ok_or(())
    }
    .boxed()
  }
}

/// A TunnelRegistry wrapper that ensures that mutations are performed sequentially,
/// using a RwLock to serialize all write operations while allowing lookups to be concurrent.
///
/// Use this when your registry would otherwise perform or evaluate requests out-of-order,
/// as a means of avoiding updates occurring before registrations complete or similar.
///
/// TODO: A more performant method would be a key-based locking mechanism on TunnelID
pub struct SerializedTunnelRegistry<TInner: ?Sized> {
  inner: Arc<tokio::sync::RwLock<Arc<TInner>>>,
}

impl<TInner> SerializedTunnelRegistry<TInner>
where
  TInner: ?Sized,
{
  pub fn new(inner: Arc<TInner>) -> Self {
    Self {
      inner: Arc::new(tokio::sync::RwLock::new(inner)),
    }
  }
}

impl<TInner> TunnelRegistry for SerializedTunnelRegistry<TInner>
where
  TInner: TunnelRegistry + Send + Sync + ?Sized,
{
  fn lookup_by_id(&self, tunnel_id: TunnelId) -> BoxFuture<Option<TunnelRecord>> {
    let inner = Arc::clone(&self.inner);
    async move {
      let lock = inner.read().await;
      lock.lookup_by_id(tunnel_id).await
    }
    .boxed()
  }

  fn lookup_by_name(&self, tunnel_name: TunnelName) -> BoxFuture<Option<TunnelRecord>> {
    let inner = Arc::clone(&self.inner);
    async move {
      let lock = inner.read().await;
      lock.lookup_by_name(tunnel_name).await
    }
    .boxed()
  }

  fn register_tunnel(
    &self,
    tunnel_id: TunnelId,
    tunnel: Arc<dyn Tunnel + Send + Sync + Unpin + 'static>,
  ) -> BoxFuture<Result<(), TunnelRegistrationError>> {
    let inner = Arc::clone(&self.inner);
    async move {
      let lock = inner.write().await;
      lock.register_tunnel(tunnel_id, tunnel).await
    }
    .boxed()
  }

  fn name_tunnel(
    &self,
    tunnel_id: TunnelId,
    name: TunnelName,
  ) -> BoxFuture<Result<(), TunnelNamingError>> {
    let inner = Arc::clone(&self.inner);
    async move {
      let lock = inner.write().await;
      lock.name_tunnel(tunnel_id, name).await
    }
    .boxed()
  }

  fn deregister_tunnel(&self, tunnel_id: TunnelId) -> BoxFuture<Result<TunnelRecord, ()>> {
    let inner = Arc::clone(&self.inner);
    async move {
      let lock = inner.write().await;
      lock.deregister_tunnel(tunnel_id).await
    }
    .boxed()
  }
}

#[derive(thiserror::Error, Debug, Clone)]
pub enum RoutingError {
  #[error("No matching tunnel could be found")]
  NoMatchingTunnel,
  #[error("The tunnel failed to provide a link")]
  LinkOpenFailure(TunnelError),
}

/// Routers are responsible for taking an address and forwarding it to
/// the appropriate tunnel. When forwarding, the router can alter the
/// address to remove any routing-specific information before it is
/// handed to the Request's protocol::Client.
pub trait Router: Downcast + DowncastSync {
  fn route(
    &self,
    //TODO: Consider taking only a [RouteAddress] here, except if other request metadata is desired
    request: &Request,
    tunnel_registry: Arc<dyn TunnelRegistry + Send + Sync>,
  ) -> BoxFuture<Result<(RouteAddress, Box<dyn TunnelStream + Send + Sync + 'static>), RoutingError>>;
}
impl_downcast!(sync Router);

#[derive(thiserror::Error, Debug)]
pub enum ClientError {
  #[error("Invalid address provided to client")]
  InvalidAddress,
  #[error("Address refused by client")]
  Refused,
  #[error("Unexpected end of stream with remote")]
  UnexpectedEnd,
  #[error("Illegal response from remote")]
  IllegalResponse(Option<Backtrace>),
}

pub trait Client {
  type Response: Send + 'static;

  fn handle(
    self,
    addr: RouteAddress,
    tunnel: Box<dyn TunnelStream + Send + 'static>,
  ) -> BoxFuture<Result<Self::Response, ClientError>>;
}

pub trait DynamicResponseClient: Send {
  fn handle_dynamic(
    self: Box<Self>,
    addr: RouteAddress,
    tunnel: Box<dyn TunnelStream + Send + 'static>,
  ) -> BoxFuture<Result<Response, ClientError>>;
}

impl<TResponse, TClient> DynamicResponseClient for TClient
where
  TClient: Client<Response = TResponse> + Send + 'static,
  TResponse: Any + Send + 'static,
  Self: Sized,
{
  fn handle_dynamic(
    self: Box<Self>,
    addr: RouteAddress,
    tunnel: Box<dyn TunnelStream + Send + 'static>,
  ) -> BoxFuture<Result<Response, ClientError>> {
    Client::handle(*self, addr, tunnel)
      .map(|result| result.map(|inner| Response::new(Box::new(inner))))
      .boxed()
  }
}

#[derive(thiserror::Error, Debug)]
pub enum ServiceError {
  #[error("Address refused by client")]
  Refused,
  #[error("Unexpected end of stream with remote")]
  UnexpectedEnd,
  #[error("Illegal response from remote")]
  IllegalResponse,
  #[error("Invalid address provided by remote client")]
  AddressError,
  #[error("An internal dependency failed")]
  DependencyFailure,
  #[error("An internal dependency failed with a backtrace")]
  BacktraceDependencyFailure(Backtrace),
  #[error(transparent)]
  InternalFailure(#[from] anyhow::Error),
}

pub trait Service {
  fn accepts(&self, addr: &RouteAddress, tunnel_id: &TunnelId) -> bool;
  // fn protocol_id() -> String where Self: Sized;

  fn handle<'a>(
    &'a self,
    addr: RouteAddress,
    stream: Box<dyn TunnelStream + Send + 'static>,
    tunnel_id: TunnelId,
  ) -> BoxFuture<'a, Result<(), ServiceError>>;
}

pub trait ServiceRegistry {
  fn find_service(
    self: Arc<Self>,
    addr: &RouteAddress,
    tunnel_id: &TunnelId,
  ) -> Option<Arc<dyn Service + Send + Sync + 'static>>;
}
