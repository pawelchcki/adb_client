use super::{
    adb_message_transport::ADBMessageTransport,
    adb_transport_message::ADBTransportMessage as Message,
    message_commands::MessageCommand as Command,
};
use crate::{ADBTimeouts, ADBTransport, Result, RustADBError};
use std::{
    collections::HashMap,
    fmt,
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

trait Writer: Send {
    fn send(&mut self, message: Message) -> Result<()>;
}
impl<T: ADBMessageTransport> Writer for T {
    fn send(&mut self, message: Message) -> Result<()> {
        self.write_message(message)
    }
}

struct Hub {
    writer: Mutex<Box<dyn Writer>>,
    disconnect: Mutex<Box<dyn ADBTransport + Send>>,
    streams: Mutex<HashMap<u32, Arc<StreamState>>>,
    targets: Mutex<HashMap<String, (SocketAddr, Arc<Rule>)>>,
    next: AtomicU32,
    closed: AtomicBool,
    max_payload: usize,
    timeout: Option<Duration>,
    error: Mutex<Option<String>>,
}

#[derive(Default)]
struct Flow {
    remote: u32,
    credit: bool,
    closed: bool,
    opening: Option<Arc<mio::Waker>>,
}
struct StreamState {
    local: u32,
    hub: Weak<Hub>,
    flow: Mutex<Flow>,
    changed: Condvar,
    incoming: Mutex<Option<SyncSender<Vec<u8>>>>,
    socket: Mutex<Option<TcpStream>>,
}

#[derive(Default)]
struct Rule {
    closed: AtomicBool,
    members: Mutex<Vec<Weak<StreamState>>>,
}
impl Rule {
    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        if let Ok(members) = self.members.lock() {
            for state in members.iter().filter_map(Weak::upgrade) {
                state.finish(true, true);
            }
        }
    }
}

fn poisoned() -> io::Error {
    io::Error::other("ADB forwarding state mutex was poisoned")
}
fn broken() -> io::Error {
    io::Error::from(io::ErrorKind::BrokenPipe)
}

impl StreamState {
    fn wait(&self, predicate: fn(&Flow) -> bool) -> io::Result<()> {
        let hub = self.hub.upgrade().ok_or_else(broken)?;
        let deadline = hub
            .timeout
            .and_then(|timeout| Instant::now().checked_add(timeout));
        let mut flow = self.flow.lock().map_err(|_| poisoned())?;
        loop {
            if predicate(&flow) {
                return Ok(());
            }
            if flow.closed {
                return Err(broken());
            }
            flow = match deadline {
                Some(deadline) => {
                    let remaining = deadline
                        .checked_duration_since(Instant::now())
                        .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
                    self.changed
                        .wait_timeout(flow, remaining)
                        .map_err(|_| poisoned())?
                        .0
                }
                None => self.changed.wait(flow).map_err(|_| poisoned())?,
            };
        }
    }

    fn finish(&self, reply: bool, abort: bool) {
        let remote = if let Ok(mut flow) = self.flow.lock() {
            let was_closed = flow.closed;
            flow.closed = true;
            self.changed.notify_all();
            if let Some(waker) = &flow.opening {
                let _ = waker.wake();
            }
            if was_closed { None } else { Some(flow.remote) }
        } else {
            None
        };
        if let Some(remote) = remote {
            if reply
                && remote != 0
                && let Some(hub) = self.hub.upgrade()
            {
                let _ = hub.send(Command::Clse, self.local, remote, &[]);
            }
            // Disconnecting the sender wakes the reader even when its bounded
            // queue is full, while preserving data already queued before EOF.
            if let Ok(mut incoming) = self.incoming.lock() {
                incoming.take();
            }
        }
        if abort
            && let Ok(socket) = self.socket.lock()
            && let Some(socket) = socket.as_ref()
        {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

struct StreamReader {
    state: Arc<StreamState>,
    incoming: Receiver<Vec<u8>>,
    bytes: Vec<u8>,
    offset: usize,
}
struct StreamWriter {
    state: Arc<StreamState>,
}

impl Read for StreamReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            if self.offset < self.bytes.len() {
                let size = output.len().min(self.bytes.len() - self.offset);
                output[..size].copy_from_slice(&self.bytes[self.offset..self.offset + size]);
                self.offset += size;
                if self.offset == self.bytes.len() {
                    self.acknowledge()?;
                }
                return Ok(size);
            }
            let hub = self.state.hub.upgrade().ok_or_else(broken)?;
            let next = match hub.timeout {
                Some(timeout) => match self.incoming.recv_timeout(timeout) {
                    Ok(bytes) => bytes,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        return Err(io::ErrorKind::TimedOut.into());
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(0),
                },
                None => match self.incoming.recv() {
                    Ok(bytes) => bytes,
                    Err(_) => return Ok(0),
                },
            };
            self.bytes = next;
            self.offset = 0;
            if self.bytes.is_empty() {
                self.acknowledge()?;
            }
        }
    }
}
impl StreamReader {
    fn acknowledge(&self) -> io::Result<()> {
        let remote = {
            let flow = self.state.flow.lock().map_err(|_| poisoned())?;
            if flow.closed {
                return Ok(());
            }
            flow.remote
        };
        self.state
            .hub
            .upgrade()
            .ok_or_else(broken)?
            .send(Command::Okay, self.state.local, remote, &[])
            .map_err(io::Error::other)?;
        Ok(())
    }
}
impl Drop for StreamReader {
    fn drop(&mut self) {
        self.state.finish(true, true);
        if let Some(hub) = self.state.hub.upgrade()
            && let Ok(mut streams) = hub.streams.lock()
        {
            streams.remove(&self.state.local);
        }
    }
}
impl Write for StreamWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let hub = self.state.hub.upgrade().ok_or_else(broken)?;
        self.state.wait(|flow| flow.credit)?;
        let remote = {
            let mut flow = self.state.flow.lock().map_err(|_| poisoned())?;
            if flow.closed {
                return Err(broken());
            }
            flow.credit = false;
            flow.remote
        };
        let size = bytes.len().min(hub.max_payload);
        hub.send(Command::Write, self.state.local, remote, &bytes[..size])
            .map_err(io::Error::other)?;
        self.state.wait(|flow| flow.credit)?;
        Ok(size)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Hub {
    fn send(&self, command: Command, local: u32, remote: u32, bytes: &[u8]) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(broken().into());
        }
        self.writer
            .lock()?
            .send(Message::try_new(command, local, remote, bytes)?)
    }
    fn record(&self, error: impl fmt::Display) {
        if let Ok(mut last) = self.error.lock() {
            *last = Some(error.to_string());
        }
    }

    fn reserve(
        self: &Arc<Self>,
        remote: u32,
        rule: &Arc<Rule>,
        socket: Option<TcpStream>,
    ) -> Result<StreamReader> {
        let mut streams = self.streams.lock()?;
        if self.closed.load(Ordering::SeqCst) || rule.closed.load(Ordering::SeqCst) {
            return Err(broken().into());
        }
        if streams.len() >= 64 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "native forwarding limit is 64 simultaneous streams",
            )
            .into());
        }
        let local = loop {
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            if id != 0 && !streams.contains_key(&id) {
                break id;
            }
        };
        // Keep each stream bounded without letting a slow reader block others.
        let (send, incoming) = mpsc::sync_channel(2);
        let state = Arc::new(StreamState {
            local,
            hub: Arc::downgrade(self),
            flow: Mutex::new(Flow {
                remote,
                credit: remote != 0,
                closed: false,
                opening: None,
            }),
            changed: Condvar::new(),
            incoming: Mutex::new(Some(send)),
            socket: Mutex::new(socket),
        });
        streams.insert(local, state.clone());
        drop(streams);
        let mut members = rule.members.lock()?;
        members.retain(|member| member.strong_count() != 0);
        members.push(Arc::downgrade(&state));
        drop(members);
        let stream = StreamReader {
            state,
            incoming,
            bytes: Vec::new(),
            offset: 0,
        };
        if rule.closed.load(Ordering::SeqCst) {
            return Err(broken().into());
        }
        Ok(stream)
    }

    fn open(
        self: &Arc<Self>,
        service: &str,
        rule: &Arc<Rule>,
        socket: Option<TcpStream>,
    ) -> Result<StreamReader> {
        if service.is_empty() || service.contains('\0') || service.len() + 1 > self.max_payload {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "invalid native ADB service").into(),
            );
        }
        let stream = self.reserve(0, rule, socket)?;
        self.start_open(service, &stream)?;
        Ok(stream)
    }

    fn start_open(&self, service: &str, stream: &StreamReader) -> Result<()> {
        let mut name = service.as_bytes().to_vec();
        name.push(0);
        self.send(Command::Open, stream.state.local, 0, &name)?;
        stream.state.wait(|flow| flow.remote != 0)?;
        Ok(())
    }

    fn start_forward_open(
        &self,
        service: &str,
        stream: &StreamReader,
        socket: &TcpStream,
    ) -> Result<()> {
        // Wait for either adbd or TCP closure without periodic socket polling.
        // Readiness monitoring leaves pre-OPEN client bytes in the socket buffer.
        socket.set_nonblocking(true)?;
        let result = (|| -> Result<()> {
            let mut poll = mio::Poll::new()?;
            let mut source = mio::net::TcpStream::from_std(socket.try_clone()?);
            poll.registry()
                .register(&mut source, mio::Token(0), mio::Interest::READABLE)?;
            let waker = Arc::new(mio::Waker::new(poll.registry(), mio::Token(1))?);
            stream.state.flow.lock()?.opening = Some(waker);
            let mut name = service.as_bytes().to_vec();
            name.push(0);
            self.send(Command::Open, stream.state.local, 0, &name)?;
            let deadline = self
                .timeout
                .and_then(|timeout| Instant::now().checked_add(timeout));
            let mut events = mio::Events::with_capacity(4);
            loop {
                {
                    let flow = stream.state.flow.lock()?;
                    if flow.closed {
                        return Err(broken().into());
                    }
                    if flow.remote != 0 {
                        return Ok(());
                    }
                }
                let timeout = deadline
                    .map(|deadline| {
                        deadline
                            .checked_duration_since(Instant::now())
                            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))
                    })
                    .transpose()?;
                match poll.poll(&mut events, timeout) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    result => result?,
                }
                for event in &events {
                    if event.token() != mio::Token(0) {
                        continue;
                    }
                    if event.is_read_closed() || event.is_write_closed() || event.is_error() {
                        return Err(broken().into());
                    }
                    if event.is_readable() {
                        match source.peek(&mut [0]) {
                            Ok(0) => return Err(broken().into()),
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                            Err(error) => return Err(error.into()),
                            Ok(_) => {}
                        }
                    }
                }
            }
        })();
        stream.state.flow.lock()?.opening = None;
        socket.set_nonblocking(false)?;
        result
    }

    fn route(self: &Arc<Self>, message: Message) -> Result<()> {
        if message.header().command() == Command::Open {
            self.incoming_open(&message);
            return Ok(());
        }
        let state = self.streams.lock()?.get(&message.header().arg1()).cloned();
        let Some(state) = state else {
            // As in AOSP, close a remote service whose OPEN acknowledgment
            // arrived after its local reader timed out or was cancelled.
            if message.header().command() == Command::Okay
                && message.header().arg0() != 0
                && message.header().arg1() != 0
            {
                self.send(
                    Command::Clse,
                    message.header().arg1(),
                    message.header().arg0(),
                    &[],
                )?;
            }
            return Ok(());
        };
        let mut flow = state.flow.lock()?;
        let remote = message.header().arg0();
        if flow.closed {
            let abandoned_open =
                flow.remote == 0 && remote != 0 && message.header().command() == Command::Okay;
            drop(flow);
            if abandoned_open {
                self.send(Command::Clse, state.local, remote, &[])?;
            }
            return Ok(());
        }
        match message.header().command() {
            Command::Okay if remote != 0 && (flow.remote == 0 || flow.remote == remote) => {
                flow.remote = remote;
                flow.credit = true;
                state.changed.notify_all();
                if let Some(waker) = &flow.opening {
                    let _ = waker.wake();
                }
            }
            Command::Write if flow.remote != 0 && flow.remote == remote => {
                let sent = state
                    .incoming
                    .lock()?
                    .as_ref()
                    .is_some_and(|incoming| incoming.try_send(message.into_payload()).is_ok());
                if !sent {
                    drop(flow);
                    state.finish(true, true);
                    self.record("peer exceeded a native stream receive window");
                }
            }
            Command::Clse if flow.remote == 0 || remote == 0 || flow.remote == remote => {
                drop(flow);
                state.finish(true, false);
            }
            Command::Okay | Command::Write | Command::Clse => {}
            command => {
                return Err(RustADBError::ADBRequestFailed(format!(
                    "unexpected forwarding command {command}"
                )));
            }
        }
        Ok(())
    }

    fn incoming_open(self: &Arc<Self>, message: &Message) {
        let name = message
            .payload()
            .strip_suffix(&[0])
            .unwrap_or(message.payload());
        let target = std::str::from_utf8(name)
            .ok()
            .and_then(|name| self.targets.lock().ok()?.get(name).cloned());
        let remote = message.header().arg0();
        let Some((address, rule)) = target.filter(|_| remote != 0 && message.header().arg1() == 0)
        else {
            let _ = self.send(Command::Clse, 0, remote, &[]);
            return;
        };
        let stream = match self.reserve(remote, &rule, None) {
            Ok(stream) => stream,
            Err(error) => {
                self.record(error);
                let _ = self.send(Command::Clse, 0, remote, &[]);
                return;
            }
        };
        let hub = self.clone();
        thread::spawn(move || {
            let prepared = (|| -> Result<TcpStream> {
                let socket = TcpStream::connect_timeout(
                    &address,
                    hub.timeout.unwrap_or(Duration::from_secs(5)),
                )?;
                socket.set_nodelay(true)?;
                *stream.state.socket.lock()? = Some(socket.try_clone()?);
                if rule.closed.load(Ordering::SeqCst) {
                    return Err(broken().into());
                }
                hub.send(Command::Okay, stream.state.local, remote, &[])?;
                Ok(socket)
            })();
            match prepared {
                Ok(socket) => proxy(socket, stream, hub.max_payload),
                Err(error) => {
                    // No OKAY has established a peer-visible local ID yet.
                    let _ = hub.send(Command::Clse, 0, remote, &[]);
                    stream.state.finish(false, true);
                    hub.record(error);
                }
            }
        });
    }

    fn stop(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(streams) = self.streams.lock() {
            for state in streams.values() {
                state.finish(false, true);
            }
        }
        // A transport clone shares the connection but not the send mutex.
        // Interrupt blocked I/O before waiting for any dispatcher or worker.
        if let Ok(mut transport) = self.disconnect.lock() {
            let _ = transport.disconnect();
        }
    }
}

fn proxy(mut socket: TcpStream, mut reader: StreamReader, max_payload: usize) {
    let Ok(mut input) = socket.try_clone() else {
        return;
    };
    let state = reader.state.clone();
    thread::spawn(move || {
        let mut writer = StreamWriter {
            state: state.clone(),
        };
        let mut buffer = vec![0; max_payload];
        let result = (|| -> io::Result<()> {
            loop {
                let size = input.read(&mut buffer)?;
                if size == 0 {
                    return Ok(());
                }
                writer.write_all(&buffer[..size])?;
            }
        })();
        if let Err(error) = result
            && let Some(hub) = state.hub.upgrade()
        {
            hub.record(error);
        }
        state.finish(true, true);
    });
    if let Err(error) = io::copy(&mut reader, &mut socket)
        && let Some(hub) = reader.state.hub.upgrade()
    {
        hub.record(error);
    }
    let _ = socket.shutdown(Shutdown::Both);
}

struct ListenerRule {
    address: SocketAddr,
    rule: Arc<Rule>,
    worker: Option<JoinHandle<()>>,
}
impl Drop for ListenerRule {
    fn drop(&mut self) {
        self.rule.close();
        let mut address = self.address;
        if address.ip().is_unspecified() {
            address.set_ip(match address.ip() {
                IpAddr::V4(_) => Ipv4Addr::LOCALHOST.into(),
                IpAddr::V6(_) => Ipv6Addr::LOCALHOST.into(),
            });
        }
        let _ = TcpStream::connect_timeout(&address, Duration::from_secs(1));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Daemon-free, multiplexed TCP forwarding over an authenticated USB/TCP device.
///
/// Convert a native device with `into_forwarder`, then add forward/reverse rules.
/// The conversion transfers ownership so ordinary commands cannot steal stream
/// packets. One dispatcher handles the connection; streams have independent
/// flow control and bounded receive queues, with at most 64 simultaneous streams.
/// Dropping the forwarder stops listeners, closes active sockets and disconnects.
pub struct ADBForwarder {
    hub: Arc<Hub>,
    listeners: HashMap<SocketAddr, ListenerRule>,
    reverse: HashMap<u16, String>,
    reader: Option<JoinHandle<()>>,
}
impl fmt::Debug for ADBForwarder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ADBForwarder")
            .field("listeners", &self.local_addresses())
            .field("reverse_ports", &self.reverse_ports())
            .finish_non_exhaustive()
    }
}

impl ADBForwarder {
    pub(crate) fn new<T: ADBMessageTransport>(
        owner: impl Send + 'static,
        mut transport: T,
        max_payload: usize,
        timeouts: ADBTimeouts,
    ) -> Self {
        let hub = Arc::new(Hub {
            writer: Mutex::new(Box::new(transport.clone())),
            disconnect: Mutex::new(Box::new(transport.clone())),
            streams: Mutex::default(),
            targets: Mutex::default(),
            next: AtomicU32::new(1),
            closed: AtomicBool::new(false),
            max_payload,
            timeout: timeouts.read,
            error: Mutex::default(),
        });
        let work = hub.clone();
        let reader = thread::spawn(move || {
            let _owner = owner;
            while !work.closed.load(Ordering::SeqCst) {
                let result = match transport.read_message_with_timeout(Duration::from_millis(500)) {
                    Ok(message) => work.route(message),
                    Err(RustADBError::IOError(error))
                        if matches!(
                            error.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                        ) =>
                    {
                        continue;
                    }
                    #[cfg(feature = "usb")]
                    Err(RustADBError::UsbError(rusb::Error::Timeout)) => continue,
                    Err(error) => Err(error),
                };
                if let Err(error) = result {
                    if !work.closed.load(Ordering::SeqCst) {
                        work.record(error);
                    }
                    break;
                }
            }
            work.stop();
        });
        Self {
            hub,
            listeners: HashMap::new(),
            reverse: HashMap::new(),
            reader: Some(reader),
        }
    }

    /// Bind a host TCP listener and open `remote` (e.g. `tcp:8001`) per connection.
    /// Port zero selects a free host port; the returned address contains that port.
    pub fn forward(&mut self, local: SocketAddr, remote: &str) -> Result<SocketAddr> {
        if remote.is_empty() || remote.contains('\0') || remote.len() + 1 > self.hub.max_payload {
            return Err(
                io::Error::new(io::ErrorKind::InvalidInput, "invalid forwarding service").into(),
            );
        }
        if self.hub.closed.load(Ordering::SeqCst) {
            return Err(broken().into());
        }
        let listener = TcpListener::bind(local)?;
        let address = listener.local_addr()?;
        let rule = Arc::new(Rule::default());
        let active = rule.clone();
        let hub = self.hub.clone();
        let remote = remote.to_owned();
        let worker = thread::spawn(move || {
            for socket in listener.incoming() {
                if active.closed.load(Ordering::SeqCst) || hub.closed.load(Ordering::SeqCst) {
                    break;
                }
                let socket = match socket {
                    Ok(socket) => socket,
                    Err(error) => {
                        hub.record(error);
                        break;
                    }
                };
                let result = (|| -> Result<()> {
                    socket.set_nodelay(true)?;
                    // Reserve before spawning so stalled handshakes cannot
                    // create more workers than the simultaneous-stream limit.
                    let reader = hub.reserve(0, &active, Some(socket.try_clone()?))?;
                    let hub = hub.clone();
                    let remote = remote.clone();
                    thread::spawn(move || {
                        match hub.start_forward_open(&remote, &reader, &socket) {
                            Ok(()) => proxy(socket, reader, hub.max_payload),
                            Err(error) => hub.record(error),
                        }
                    });
                    Ok(())
                })();
                if let Err(error) = result {
                    hub.record(error);
                }
            }
        });
        self.listeners.insert(
            address,
            ListenerRule {
                address,
                rule,
                worker: Some(worker),
            },
        );
        Ok(address)
    }

    /// Remove one host listener and close its active streams.
    pub fn forward_remove(&mut self, local: SocketAddr) -> Result<()> {
        self.listeners
            .remove(&local)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        Ok(())
    }
    /// Remove every host listener and its active streams.
    pub fn forward_remove_all(&mut self) {
        self.listeners.clear();
    }
    /// Host addresses of current forward listeners.
    #[must_use]
    pub fn local_addresses(&self) -> Vec<SocketAddr> {
        self.listeners.keys().copied().collect()
    }
    /// Device TCP ports of current reverse listeners.
    #[must_use]
    pub fn reverse_ports(&self) -> Vec<u16> {
        self.reverse.keys().copied().collect()
    }
    /// Most recent transport or per-connection error, if any.
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        self.hub.error.lock().ok()?.clone()
    }

    /// Ask adbd to listen on a device TCP port and route connections to `local`.
    /// Port zero asks adbd for an available port. Only registered local targets
    /// may be opened by the device; arbitrary device-initiated services are refused.
    pub fn reverse(&mut self, remote_port: u16, local: SocketAddr) -> Result<u16> {
        if local.port() == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "reverse target port cannot be zero",
            )
            .into());
        }
        let target = format!("tcp:{}", local.port());
        let rule = Arc::new(Rule::default());
        {
            let mut targets = self.hub.targets.lock()?;
            if targets.contains_key(&target) || self.reverse.contains_key(&remote_port) {
                return Err(io::Error::from(io::ErrorKind::AddrInUse).into());
            }
            targets.insert(target.clone(), (local, rule));
        }
        let response = self.control(&format!("reverse:forward:tcp:{remote_port};{target}"));
        let port = response.and_then(|bytes| {
            if remote_port != 0 {
                return Ok(remote_port);
            }
            if bytes.len() < 4 {
                return Err(RustADBError::ADBRequestFailed(
                    "reverse response omitted allocated port".into(),
                ));
            }
            let size = usize::from_str_radix(std::str::from_utf8(&bytes[..4])?, 16)?;
            let port = bytes
                .get(4..4 + size)
                .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
            let port: u16 = std::str::from_utf8(port)?.parse()?;
            if port == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "adbd returned reverse port zero",
                )
                .into());
            }
            Ok(port)
        });
        match port {
            Ok(port) => {
                self.reverse.insert(port, target);
                Ok(port)
            }
            Err(error) => {
                if let Some((_, rule)) = self.hub.targets.lock()?.remove(&target) {
                    rule.close();
                }
                Err(error)
            }
        }
    }

    fn control(&self, service: &str) -> Result<Vec<u8>> {
        let mut reader = self.hub.open(service, &Arc::new(Rule::default()), None)?;
        let mut reply = Vec::new();
        reader.by_ref().take(4097).read_to_end(&mut reply)?;
        if reply.len() > 4096 {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "oversized reverse response").into(),
            );
        }
        if !reply.starts_with(b"OKAY") {
            return Err(RustADBError::ADBRequestFailed(
                String::from_utf8_lossy(&reply).into_owned(),
            ));
        }
        Ok(reply[4..].to_vec())
    }

    /// Remove one device reverse listener and close its active streams.
    pub fn reverse_remove(&mut self, remote_port: u16) -> Result<()> {
        if !self.reverse.contains_key(&remote_port) {
            return Err(io::Error::from(io::ErrorKind::NotFound).into());
        }
        self.control(&format!("reverse:killforward:tcp:{remote_port}"))?;
        if let Some(target) = self.reverse.remove(&remote_port)
            && let Some((_, rule)) = self.hub.targets.lock()?.remove(&target)
        {
            rule.close();
        }
        Ok(())
    }
    /// Remove every reverse listener created by this forwarder.
    pub fn reverse_remove_all(&mut self) -> Result<()> {
        for port in self.reverse_ports() {
            self.reverse_remove(port)?;
        }
        Ok(())
    }
    /// Stop listeners and streams, disconnect, and wait for the packet dispatcher.
    pub fn close(mut self) -> Result<()> {
        // adbd registers reverse listeners against this transport and removes
        // them on disconnect. Sending control requests here could block behind
        // the very writer that shutdown must interrupt.
        self.hub.stop();
        self.forward_remove_all();
        self.reverse.clear();
        if let Some(reader) = self.reader.take() {
            reader
                .join()
                .map_err(|_| io::Error::other("ADB dispatcher panicked"))?;
        }
        Ok(())
    }
}
impl Drop for ADBForwarder {
    fn drop(&mut self) {
        self.hub.stop();
        self.forward_remove_all();
    }
}

#[cfg(test)]
mod tests;
