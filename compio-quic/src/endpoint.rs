use std::{
    cell::RefCell,
    collections::HashMap,
    io,
    mem::ManuallyDrop,
    net::{SocketAddr, SocketAddrV6},
    pin::pin,
    rc::Rc,
    sync::Arc,
    task::Poll,
    time::Instant,
};

use compio_buf::BufResult;
use compio_net::UdpSocket;
use compio_runtime::JoinHandle;
use flume::{unbounded, Receiver, Sender};
use futures_util::{
    future::{join_all, poll_fn},
    select,
    task::AtomicWaker,
    FutureExt,
};
use quinn_proto::{
    ClientConfig, ConnectError, ConnectionHandle, DatagramEvent, EndpointConfig, EndpointEvent,
    ServerConfig, Transmit, VarInt,
};

use crate::{
    connection::{Connecting, ConnectionEvent},
    socket::{RecvMeta, Socket},
};

#[derive(Debug)]
struct EndpointState {
    endpoint: quinn_proto::Endpoint,
    worker: Option<JoinHandle<()>>,
    connections: HashMap<ConnectionHandle, Sender<ConnectionEvent>>,
    close: Option<(VarInt, String)>,
}

impl EndpointState {
    fn new(config: EndpointConfig, server_config: Option<ServerConfig>, allow_mtud: bool) -> Self {
        Self {
            endpoint: quinn_proto::Endpoint::new(
                Arc::new(config),
                server_config.map(Arc::new),
                allow_mtud,
                None,
            ),
            worker: None,
            connections: HashMap::new(),
            close: None,
        }
    }

    fn handle_data(&mut self, meta: RecvMeta, buf: &[u8]) -> Vec<(Vec<u8>, Transmit)> {
        let mut outgoing = vec![];
        let now = Instant::now();
        for data in buf[..meta.len]
            .chunks(meta.stride.min(meta.len))
            .map(Into::into)
        {
            let mut resp_buf = Vec::new();
            match self.endpoint.handle(
                now,
                meta.remote,
                meta.local_ip,
                meta.ecn,
                data,
                &mut resp_buf,
            ) {
                Some(DatagramEvent::NewConnection(_incoming)) => todo!("server"),
                Some(DatagramEvent::ConnectionEvent(ch, event)) => {
                    let _ = self
                        .connections
                        .get(&ch)
                        .unwrap()
                        .send(ConnectionEvent::Proto(event));
                }
                Some(DatagramEvent::Response(transmit)) => outgoing.push((resp_buf, transmit)),
                None => {}
            }
        }
        outgoing
    }

    fn handle_event(&mut self, ch: ConnectionHandle, event: EndpointEvent) -> bool {
        let mut is_idle = false;
        if event.is_drained() {
            self.connections.remove(&ch);
            if self.connections.is_empty() {
                is_idle = true;
            }
        }
        if let Some(event) = self.endpoint.handle_event(ch, event) {
            let _ = self
                .connections
                .get(&ch)
                .unwrap()
                .send(ConnectionEvent::Proto(event));
        }
        is_idle
    }
}

#[derive(Debug)]
struct EndpointInner {
    state: RefCell<EndpointState>,
    socket: Socket,
    ipv6: bool,
    events_tx: Sender<(ConnectionHandle, EndpointEvent)>,
    events_rx: Receiver<(ConnectionHandle, EndpointEvent)>,
    done: AtomicWaker,
}

impl EndpointInner {
    fn new(
        socket: UdpSocket,
        config: EndpointConfig,
        server_config: Option<ServerConfig>,
    ) -> io::Result<Self> {
        let socket = Socket::new(socket)?;
        let ipv6 = socket.local_addr()?.is_ipv6();
        let allow_mtud = !socket.may_fragment();

        let (events_tx, events_rx) = unbounded();

        Ok(Self {
            state: RefCell::new(EndpointState::new(config, server_config, allow_mtud)),
            socket,
            ipv6,
            events_tx,
            events_rx,
            done: AtomicWaker::new(),
        })
    }

    fn connect(
        &self,
        remote: SocketAddr,
        server_name: &str,
        config: ClientConfig,
    ) -> Result<Connecting, ConnectError> {
        let (handle, conn) = {
            let mut state = self.state.borrow_mut();

            if state.worker.is_none() {
                return Err(ConnectError::EndpointStopping);
            }
            if remote.is_ipv6() && !self.ipv6 {
                return Err(ConnectError::InvalidRemoteAddress(remote));
            }
            let remote = if self.ipv6 {
                SocketAddr::V6(match remote {
                    SocketAddr::V4(addr) => {
                        SocketAddrV6::new(addr.ip().to_ipv6_mapped(), addr.port(), 0, 0)
                    }
                    SocketAddr::V6(addr) => addr,
                })
            } else {
                remote
            };

            state
                .endpoint
                .connect(Instant::now(), config, remote, server_name)?
        };

        Ok(self.new_connection(handle, conn))
    }

    fn new_connection(
        &self,
        handle: ConnectionHandle,
        conn: quinn_proto::Connection,
    ) -> Connecting {
        let (tx, rx) = unbounded();

        let mut state = self.state.borrow_mut();
        if let Some((error_code, reason)) = &state.close {
            tx.send(ConnectionEvent::Close(*error_code, reason.clone()))
                .unwrap();
        }
        state.connections.insert(handle, tx);
        Connecting::new(
            handle,
            conn,
            self.socket.clone(),
            self.events_tx.clone(),
            rx,
        )
    }

    async fn run(&self) -> io::Result<()> {
        let mut recv_fut = pin!(
            self.socket
                .recv(Vec::with_capacity(
                    self.state
                        .borrow()
                        .endpoint
                        .config()
                        .get_max_udp_payload_size()
                        .min(64 * 1024) as usize
                        * self.socket.max_gro_segments(),
                ))
                .fuse()
        );

        let idle = AtomicWaker::new();
        let mut close_fut = poll_fn(|cx| {
            let state = self.state.borrow();
            if state.close.is_some() && state.connections.is_empty() {
                Poll::Ready(())
            } else {
                idle.register(cx.waker());
                Poll::Pending
            }
        })
        .fuse();

        loop {
            select! {
                BufResult(res, recv_buf) = recv_fut => {
                    match res {
                        Ok(meta) => {
                            let outgoing = self.state.borrow_mut().handle_data(meta, &recv_buf);
                            join_all(
                                outgoing.into_iter().map(|(buf, transmit)| async move {
                                    self.socket.send(buf, &transmit).await
                                }),
                            )
                            .await;
                        }
                        Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
                        Err(e) => break Err(e),
                    }
                    recv_fut.set(self.socket.recv(recv_buf).fuse());
                },
                (ch, event) = self.events_rx.recv_async().map(Result::unwrap) => {
                    if self.state.borrow_mut().handle_event(ch, event) {
                        idle.wake();
                    }
                },
                _ = close_fut => break Ok(()),
            }
        }
    }
}

/// A QUIC endpoint.
#[derive(Debug, Clone)]
pub struct Endpoint {
    inner: Rc<EndpointInner>,
    /// The client configuration used by `connect`
    pub default_client_config: Option<ClientConfig>,
}

impl Endpoint {
    /// Create a QUIC endpoint.
    pub fn new(
        socket: UdpSocket,
        config: EndpointConfig,
        server_config: Option<ServerConfig>,
        default_client_config: Option<ClientConfig>,
    ) -> io::Result<Self> {
        let inner = Rc::new(EndpointInner::new(socket, config, server_config)?);
        let worker = compio_runtime::spawn({
            let inner = inner.clone();
            async move { inner.run().await.unwrap() }
        });
        inner.state.borrow_mut().worker = Some(worker);
        Ok(Self {
            inner,
            default_client_config,
        })
    }

    /// Connect to a remote endpoint.
    pub fn connect(
        &self,
        remote: SocketAddr,
        server_name: &str,
        config: Option<ClientConfig>,
    ) -> Result<Connecting, ConnectError> {
        let config = if let Some(config) = config {
            config
        } else if let Some(config) = &self.default_client_config {
            config.clone()
        } else {
            return Err(ConnectError::NoDefaultClientConfig);
        };

        self.inner.connect(remote, server_name, config)
    }

    // Modified from [`SharedFd::try_unwrap_inner`], see notes there.
    unsafe fn try_unwrap_inner(this: &ManuallyDrop<Self>) -> Option<EndpointInner> {
        let ptr = ManuallyDrop::new(std::ptr::read(&this.inner));
        match Rc::try_unwrap(ManuallyDrop::into_inner(ptr)) {
            Ok(inner) => Some(inner),
            Err(ptr) => {
                std::mem::forget(ptr);
                None
            }
        }
    }

    /// Shutdown the endpoint.
    ///
    /// This will close all connections and the underlying socket. Note that it
    /// will wait for all connections and all clones of the endpoint (and any
    /// clone of the underlying socket) to be dropped before closing the socket.
    ///
    /// See [`Connection::close()`](crate::Connection::close) for details.
    pub async fn close(self, error_code: VarInt, reason: &str) -> io::Result<()> {
        let reason = reason.to_string();
        self.inner.state.borrow_mut().close = Some((error_code, reason.clone()));
        for conn in self.inner.state.borrow().connections.values() {
            let _ = conn.send(ConnectionEvent::Close(error_code, reason.clone()));
        }

        let worker = self.inner.state.borrow_mut().worker.take();
        if let Some(worker) = worker {
            let _ = worker.await;
        }

        let this = ManuallyDrop::new(self);
        let inner = poll_fn(move |cx| {
            if let Some(inner) = unsafe { Self::try_unwrap_inner(&this) } {
                Poll::Ready(inner)
            } else {
                this.inner.done.register(cx.waker());
                Poll::Pending
            }
        })
        .await;

        inner.socket.close().await
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        if Rc::strong_count(&self.inner) == 2 {
            self.inner.done.wake();
        }
    }
}
