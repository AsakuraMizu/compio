use std::{
    cell::RefCell,
    io,
    net::{IpAddr, SocketAddr},
    pin::{pin, Pin},
    rc::Rc,
    task::{Context, Poll},
    time::Instant,
};

use compio_buf::BufResult;
use compio_runtime::JoinHandle;
use flume::{Receiver, Sender};
use futures_util::{
    future::{poll_fn, Fuse, FusedFuture, Future, LocalBoxFuture},
    select,
    task::AtomicWaker,
    FutureExt,
};
use quinn_proto::{
    crypto::rustls::HandshakeData, ConnectionError, ConnectionHandle, EndpointEvent, VarInt,
};

use crate::socket::Socket;

#[derive(Debug)]
pub enum ConnectionEvent {
    Close(VarInt, String),
    Proto(quinn_proto::ConnectionEvent),
}

#[derive(Debug)]
struct ConnectionState {
    conn: quinn_proto::Connection,
    connected: bool,
    error: Option<ConnectionError>,
    worker: Option<JoinHandle<()>>,
}

impl ConnectionState {
    fn new(conn: quinn_proto::Connection) -> Self {
        Self {
            conn,
            connected: false,
            error: None,
            worker: None,
        }
    }

    fn terminate(&mut self, reason: ConnectionError) {
        self.error = Some(reason);
        self.connected = false;
    }
}

#[derive(Debug)]
struct ConnectionInner {
    state: RefCell<ConnectionState>,
    handle: ConnectionHandle,
    socket: Socket,
    on_connected: AtomicWaker,
    on_handshake_data: AtomicWaker,
    endpoint_tx: Sender<(ConnectionHandle, EndpointEvent)>,
    conn_rx: Receiver<ConnectionEvent>,
}

impl ConnectionInner {
    fn new(
        handle: ConnectionHandle,
        conn: quinn_proto::Connection,
        socket: Socket,
        endpoint_tx: Sender<(ConnectionHandle, EndpointEvent)>,
        conn_rx: Receiver<ConnectionEvent>,
    ) -> Self {
        Self {
            state: RefCell::new(ConnectionState::new(conn)),
            handle,
            socket,
            on_connected: AtomicWaker::new(),
            on_handshake_data: AtomicWaker::new(),
            endpoint_tx,
            conn_rx,
        }
    }

    fn on_close(&self) {
        self.on_handshake_data.wake();
        self.on_connected.wake();
    }

    fn close(&self, error_code: VarInt, reason: String) {
        let mut state = self.state.borrow_mut();
        state.conn.close(Instant::now(), error_code, reason.into());
        state.terminate(ConnectionError::LocallyClosed);
        self.on_close();
    }

    async fn run(&self) -> io::Result<()> {
        let mut send_buf = Some(Vec::with_capacity(
            self.state.borrow().conn.current_mtu() as usize
        ));
        let mut transmit_fut = pin!(Fuse::terminated());

        let mut timer = Timer::new();

        loop {
            {
                let now = Instant::now();
                let mut state = self.state.borrow_mut();

                if let Some(mut buf) = send_buf.take() {
                    if let Some(transmit) =
                        state
                            .conn
                            .poll_transmit(now, self.socket.max_gso_segments(), &mut buf)
                    {
                        transmit_fut
                            .set(async move { self.socket.send(buf, &transmit).await }.fuse())
                    } else {
                        send_buf = Some(buf);
                    }
                }

                timer.reset(state.conn.poll_timeout());

                while let Some(event) = state.conn.poll_endpoint_events() {
                    let _ = self.endpoint_tx.send((self.handle, event));
                }

                while let Some(event) = state.conn.poll() {
                    use quinn_proto::Event::*;
                    match event {
                        HandshakeDataReady => {
                            self.on_handshake_data.wake();
                        }
                        Connected => {
                            state.connected = true;
                            self.on_connected.wake();
                        }
                        ConnectionLost { reason } => {
                            state.terminate(reason);
                            self.on_close();
                        }
                        _ => {}
                    }
                }

                if state.conn.is_drained() {
                    break Ok(());
                }
            }

            select! {
                _ = timer => {
                    self.state.borrow_mut().conn.handle_timeout(Instant::now());
                    timer.reset(None);
                },
                ev = self.conn_rx.recv_async() =>match ev {
                    Ok(ConnectionEvent::Close(error_code, reason)) => self.close(error_code, reason),
                    Ok(ConnectionEvent::Proto(ev)) => self.state.borrow_mut().conn.handle_event(ev),
                    Err(_) => unreachable!("endpoint dropped connection"),
                },
                BufResult(res, mut buf) = transmit_fut => match res {
                    Ok(()) => {
                        buf.clear();
                        send_buf = Some(buf);
                    },
                    Err(e) => break Err(e),
                },
            }
        }
    }
}

macro_rules! conn_fn {
    () => {
        /// The local IP address which was used when the peer established
        /// the connection.
        ///
        /// This can be different from the address the endpoint is bound to, in case
        /// the endpoint is bound to a wildcard address like `0.0.0.0` or `::`.
        ///
        /// This will return `None` for clients, or when the platform does not
        /// expose this information.
        pub fn local_ip(&self) -> Option<IpAddr> {
            self.0.state.borrow().conn.local_ip()
        }

        /// The peer's UDP address.
        ///
        /// Will panic if called after `poll` has returned `Ready`.
        pub fn remote_address(&self) -> SocketAddr {
            self.0.state.borrow().conn.remote_address()
        }

        /// Parameters negotiated during the handshake.
        pub async fn handshake_data(&mut self) -> Result<Box<HandshakeData>, ConnectionError> {
            poll_fn(|cx| {
                let state = self.0.state.borrow();
                if let Some(handshake_data) = state.conn.crypto_session().handshake_data() {
                    Poll::Ready(Ok(handshake_data.downcast::<HandshakeData>().unwrap()))
                } else if let Some(ref error) = state.error {
                    Poll::Ready(Err(error.clone()))
                } else {
                    self.0.on_handshake_data.register(cx.waker());
                    Poll::Pending
                }
            })
            .await
        }
    };
}

/// In-progress connection attempt future
#[derive(Debug)]
#[must_use = "futures/streams/sinks do nothing unless you `.await` or poll them"]
pub struct Connecting(Rc<ConnectionInner>);

impl Connecting {
    conn_fn!();

    pub(crate) fn new(
        handle: ConnectionHandle,
        conn: quinn_proto::Connection,
        socket: Socket,
        endpoint_tx: Sender<(ConnectionHandle, EndpointEvent)>,
        conn_rx: Receiver<ConnectionEvent>,
    ) -> Self {
        let inner = Rc::new(ConnectionInner::new(
            handle,
            conn,
            socket,
            endpoint_tx,
            conn_rx,
        ));
        let worker = compio_runtime::spawn({
            let inner = inner.clone();
            async move { inner.run().await.unwrap() }
        });
        inner.state.borrow_mut().worker = Some(worker);
        Self(inner)
    }
}

impl Future for Connecting {
    type Output = Result<Connection, ConnectionError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let state = self.0.state.borrow();
        if state.connected {
            Poll::Ready(Ok(Connection(self.0.clone())))
        } else if let Some(error) = &state.error {
            Poll::Ready(Err(error.clone()))
        } else {
            self.0.on_connected.register(cx.waker());
            Poll::Pending
        }
    }
}

/// A QUIC connection.
#[derive(Debug)]
pub struct Connection(Rc<ConnectionInner>);

impl Connection {
    conn_fn!();

    /// Close the connection immediately.
    ///
    /// Pending operations will fail immediately with
    /// [`ConnectionError::LocallyClosed`]. Delivery of data on unfinished
    /// streams is not guaranteed, so the application must call this only when
    /// all important communications have been completed, e.g. by calling
    /// [`finish`] on outstanding [`SendStream`]s and waiting for the resulting
    /// futures to complete.
    ///
    /// `error_code` and `reason` are not interpreted, and are provided directly
    /// to the peer.
    ///
    /// `reason` will be truncated to fit in a single packet with overhead; to
    /// improve odds that it is preserved in full, it should be kept under 1KiB.
    ///
    /// [`ConnectionError::LocallyClosed`]: quinn_proto::ConnectionError::LocallyClosed
    /// [`finish`]: crate::SendStream::finish
    /// [`SendStream`]: crate::SendStream
    pub fn close(&self, error_code: VarInt, reason: &str) {
        self.0.close(error_code, reason.to_string());
    }

    /// Wait for the connection to be closed for any reason
    pub async fn closed(&self) -> ConnectionError {
        let worker = self.0.state.borrow_mut().worker.take();
        if let Some(worker) = worker {
            let _ = worker.await;
        }

        self.0.state.borrow().error.clone().unwrap()
    }
}

struct Timer {
    deadline: Option<Instant>,
    fut: Fuse<LocalBoxFuture<'static, ()>>,
}

impl Timer {
    fn new() -> Self {
        Self {
            deadline: None,
            fut: Fuse::terminated(),
        }
    }

    fn reset(&mut self, deadline: Option<Instant>) {
        if let Some(deadline) = deadline {
            if self.deadline.is_none() || self.deadline != Some(deadline) {
                self.fut = compio_runtime::time::sleep_until(deadline)
                    .boxed_local()
                    .fuse();
            }
        } else {
            self.fut = Fuse::terminated();
        }
        self.deadline = deadline;
    }
}

impl Future for Timer {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.fut.poll_unpin(cx)
    }
}

impl FusedFuture for Timer {
    fn is_terminated(&self) -> bool {
        self.fut.is_terminated()
    }
}
