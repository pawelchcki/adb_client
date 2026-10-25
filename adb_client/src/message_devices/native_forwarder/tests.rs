use super::*;
use std::sync::Barrier;

#[derive(Clone, Debug)]
struct FakeTransport {
    incoming: Arc<Mutex<Receiver<Message>>>,
    replies: mpsc::Sender<Message>,
    outgoing: mpsc::Sender<Message>,
    disconnected: Arc<AtomicBool>,
}

// Production keeps the original ADBMessageDevice alive in the dispatcher;
// its Drop disconnects even if a concurrent writer held the hub's send lock.
struct ConnectionOwner(FakeTransport);
impl Drop for ConnectionOwner {
    fn drop(&mut self) {
        let _ = ADBTransport::disconnect(&mut self.0);
    }
}
impl ADBTransport for FakeTransport {
    fn connect(&mut self) -> Result<()> {
        Ok(())
    }
    fn disconnect(&mut self) -> Result<()> {
        self.disconnected.store(true, Ordering::SeqCst);
        let _ = self.replies.send(packet(Command::Clse, 0, 0, &[]));
        Ok(())
    }
}
impl ADBMessageTransport for FakeTransport {
    fn read_message_with_timeout(&mut self, timeout: Duration) -> Result<Message> {
        self.incoming
            .lock()?
            .recv_timeout(timeout)
            .map_err(|_| io::Error::from(io::ErrorKind::TimedOut).into())
    }
    fn write_message_with_timeout(&mut self, message: Message, _: Duration) -> Result<()> {
        self.outgoing.send(message).map_err(|_| broken().into())
    }
}
fn packet(command: Command, remote: u32, local: u32, bytes: &[u8]) -> Message {
    Message::try_new(command, remote, local, bytes).unwrap()
}
fn fixture() -> (
    ADBForwarder,
    mpsc::Sender<Message>,
    Receiver<Message>,
    Arc<AtomicBool>,
) {
    fixture_with_timeout(Some(Duration::from_secs(3)))
}

fn fixture_with_timeout(
    read: Option<Duration>,
) -> (
    ADBForwarder,
    mpsc::Sender<Message>,
    Receiver<Message>,
    Arc<AtomicBool>,
) {
    let (replies, incoming) = mpsc::channel();
    let (outgoing, received) = mpsc::channel();
    let disconnected = Arc::new(AtomicBool::new(false));
    let transport = FakeTransport {
        incoming: Arc::new(Mutex::new(incoming)),
        replies: replies.clone(),
        outgoing,
        disconnected: disconnected.clone(),
    };
    (
        ADBForwarder::new(
            ConnectionOwner(transport.clone()),
            transport,
            4096,
            ADBTimeouts {
                read,
                ..ADBTimeouts::default()
            },
        ),
        replies,
        received,
        disconnected,
    )
}

#[test]
fn simultaneous_forward_clients_are_isolated_and_flow_controlled() {
    let (mut controller, replies, received, disconnected) = fixture();
    let stopping = disconnected.clone();
    let peer = thread::spawn(move || {
        let mut active = HashMap::new();
        let mut pending: HashMap<u32, std::collections::VecDeque<Vec<u8>>> = HashMap::new();
        let mut in_flight = std::collections::HashSet::new();
        let mut writes = 0;
        while !stopping.load(Ordering::SeqCst) {
            let Ok(message) = received.recv_timeout(Duration::from_millis(50)) else {
                continue;
            };
            let local = message.header().arg0();
            match message.header().command() {
                Command::Open => {
                    assert_eq!(message.payload(), b"tcp:7000\0");
                    let remote = 1000 + local;
                    active.insert(local, remote);
                    replies
                        .send(packet(Command::Okay, remote, local, &[]))
                        .unwrap();
                }
                Command::Write => {
                    let remote = active[&local];
                    assert_eq!(message.header().arg1(), remote);
                    assert!(message.payload().len() <= 4096);
                    let queue = pending.entry(local).or_default();
                    queue.push_back(message.into_payload());
                    // Cross data with its ACK, but permit only one unacknowledged
                    // echo packet per stream, exactly like a classic ADB peer.
                    if in_flight.insert(local) {
                        replies
                            .send(packet(
                                Command::Write,
                                remote,
                                local,
                                &queue.pop_front().unwrap(),
                            ))
                            .unwrap();
                    }
                    replies
                        .send(packet(Command::Okay, remote, local, &[]))
                        .unwrap();
                    writes += 1;
                }
                Command::Clse => {
                    let _ =
                        replies.send(packet(Command::Clse, message.header().arg1(), local, &[]));
                }
                Command::Okay => {
                    assert_eq!(message.header().arg1(), active[&local]);
                    assert!(in_flight.remove(&local));
                    if let Some(bytes) = pending
                        .get_mut(&local)
                        .and_then(std::collections::VecDeque::pop_front)
                    {
                        in_flight.insert(local);
                        replies
                            .send(packet(Command::Write, active[&local], local, &bytes))
                            .unwrap();
                    }
                }
                other => panic!("unexpected {other}"),
            }
        }
        assert!(active.len() >= 2);
        assert!(writes > 2);
    });
    let address = controller
        .forward("127.0.0.1:0".parse().unwrap(), "tcp:7000")
        .unwrap();
    let ready = Arc::new(Barrier::new(3));
    let clients: Vec<_> = b"ab"
        .iter()
        .copied()
        .map(|tag| {
            let ready = ready.clone();
            thread::spawn(move || {
                let mut socket = TcpStream::connect(address).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                ready.wait();
                // Larger than a packet and distinct for each connection.
                let bytes = vec![tag; 256 * 1024];
                socket.write_all(&bytes).unwrap();
                let mut echoed = vec![0; bytes.len()];
                socket.read_exact(&mut echoed).unwrap();
                assert_eq!(echoed, bytes);
            })
        })
        .collect();
    ready.wait();
    for client in clients {
        client.join().unwrap();
    }
    controller.forward_remove(address).unwrap();
    assert_eq!(controller.local_addresses(), [] as [SocketAddr; 0]);
    assert!(TcpStream::connect(address).is_err());
    controller.close().unwrap();
    peer.join().unwrap();
    assert!(disconnected.load(Ordering::SeqCst));
}

#[test]
fn closing_a_rule_cancels_a_pending_open_and_its_host_socket() {
    let (mut controller, _replies, received, disconnected) = fixture();
    let address = controller
        .forward("127.0.0.1:0".parse().unwrap(), "tcp:7000")
        .unwrap();
    let mut socket = TcpStream::connect(address).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    assert_eq!(
        received
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .header()
            .command(),
        Command::Open
    );
    // The peer never acknowledges OPEN. Cancellation must still wake the accept
    // worker, close the client and allow the dispatcher to shut down.
    controller.close().unwrap();
    let mut byte = [0];
    assert_eq!(socket.read(&mut byte).unwrap(), 0);
    assert!(disconnected.load(Ordering::SeqCst));
}

#[test]
fn reverse_routes_only_registered_targets_and_can_remove_an_individual_rule() {
    let (mut controller, replies, received, disconnected) = fixture();
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = target.local_addr().unwrap();
    let host = thread::spawn(move || {
        let (mut stream, _) = target.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = [0; 7];
        stream.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"request");
        stream.write_all(b"response").unwrap();
        // Keep the socket alive until adbd closes the logical stream.
        let mut tail = Vec::new();
        stream.read_to_end(&mut tail).unwrap();
    });
    let stopping = disconnected.clone();
    let wire = replies.clone();
    let (response_tx, response_rx) = mpsc::channel();
    let (refused_tx, refused_rx) = mpsc::channel();
    let peer = thread::spawn(move || {
        let mut controls = HashMap::new();
        let mut reverse_local = None;
        while !stopping.load(Ordering::SeqCst) {
            let Ok(message) = received.recv_timeout(Duration::from_millis(50)) else {
                continue;
            };
            let local = message.header().arg0();
            let remote = message.header().arg1();
            match message.header().command() {
                Command::Open => {
                    let response = if message.payload()
                        == format!("reverse:forward:tcp:0;tcp:{}\0", address.port()).as_bytes()
                    {
                        b"OKAY000543210".as_slice()
                    } else {
                        assert_eq!(message.payload(), b"reverse:killforward:tcp:43210\0");
                        b"OKAY".as_slice()
                    };
                    controls.insert(local, 100 + local);
                    wire.send(packet(Command::Okay, 100 + local, local, &[]))
                        .unwrap();
                    wire.send(packet(Command::Write, 100 + local, local, response))
                        .unwrap();
                }
                Command::Okay if remote == 900 && reverse_local.is_none() => {
                    reverse_local = Some(local);
                    wire.send(packet(Command::Write, 900, local, b"request"))
                        .unwrap();
                }
                Command::Okay if controls.contains_key(&local) => {
                    wire.send(packet(Command::Clse, remote, local, &[]))
                        .unwrap();
                }
                Command::Write => {
                    assert_eq!(Some(local), reverse_local);
                    assert_eq!(remote, 900);
                    assert_eq!(message.payload(), b"response");
                    wire.send(packet(Command::Okay, 900, local, &[])).unwrap();
                    wire.send(packet(Command::Clse, 900, local, &[])).unwrap();
                    response_tx.send(()).unwrap();
                }
                Command::Clse if remote == 901 => {
                    assert_eq!(local, 0);
                    refused_tx.send(()).unwrap();
                }
                Command::Clse | Command::Okay => {}
                other => panic!("unexpected {other}"),
            }
        }
    });
    assert_eq!(controller.reverse(0, address).unwrap(), 43210);
    assert_eq!(controller.reverse_ports(), [43210]);
    let service = format!("tcp:{}\0", address.port());
    replies
        .send(packet(Command::Open, 900, 0, service.as_bytes()))
        .unwrap();
    response_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    host.join().unwrap();
    controller.reverse_remove(43210).unwrap();
    assert_eq!(controller.reverse_ports(), [] as [u16; 0]);
    replies
        .send(packet(Command::Open, 901, 0, service.as_bytes()))
        .unwrap();
    refused_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    controller.close().unwrap();
    peer.join().unwrap();
}

#[test]
fn stalled_open_does_not_block_other_clients_on_the_same_rule() {
    let (mut controller, replies, received, _) = fixture_with_timeout(None);
    let address = controller
        .forward("127.0.0.1:0".parse().unwrap(), "tcp:7000")
        .unwrap();
    let _first = TcpStream::connect(address).unwrap();
    let first = received.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(first.header().command(), Command::Open);
    let mut second = TcpStream::connect(address).unwrap();
    second
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let opened = received.recv_timeout(Duration::from_secs(2));
    if let Ok(opened) = opened {
        assert_eq!(opened.header().command(), Command::Open);
        assert_ne!(opened.header().arg0(), first.header().arg0());
        let local = opened.header().arg0();
        replies.send(packet(Command::Okay, 42, local, &[])).unwrap();
        replies
            .send(packet(Command::Write, 42, local, b"ready"))
            .unwrap();
        let mut bytes = [0; 5];
        second.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"ready");
    } else {
        controller.close().unwrap();
        panic!("a stalled OPEN blocked the next client");
    }
    controller.close().unwrap();
}

#[test]
fn late_open_acknowledgments_close_the_abandoned_remote_stream() {
    let (controller, _, received, _) = fixture_with_timeout(None);
    let rule = Arc::new(Rule::default());
    let stream = controller.hub.reserve(0, &rule, None).unwrap();
    let local = stream.state.local;
    // Cancellation may race the open worker's destruction of its reader.
    rule.close();
    controller
        .hub
        .route(packet(Command::Okay, 42, local, &[]))
        .unwrap();
    let closed_while_present = received.recv_timeout(Duration::from_millis(200));
    drop(stream);
    controller
        .hub
        .route(packet(Command::Okay, 43, local, &[]))
        .unwrap();
    let closed_after_drop = received.recv_timeout(Duration::from_millis(200));
    controller.close().unwrap();
    for (message, remote) in [(closed_while_present, 42), (closed_after_drop, 43)] {
        let message = message.expect("late OPEN acknowledgment was ignored");
        assert_eq!(message.header().command(), Command::Clse);
        assert_eq!(message.header().arg0(), local);
        assert_eq!(message.header().arg1(), remote);
    }
}

#[test]
fn receive_window_overflow_wakes_an_unlimited_reader() {
    let (controller, _, _, _) = fixture_with_timeout(None);
    let rule = Arc::new(Rule::default());
    let mut stream = controller.hub.reserve(42, &rule, None).unwrap();
    let local = stream.state.local;
    for _ in 0..3 {
        controller
            .hub
            .route(packet(Command::Write, 42, local, b"data"))
            .unwrap();
    }
    let (done, result) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = stream.read_to_end(&mut bytes);
        done.send((read, bytes)).unwrap();
    });
    let outcome = result.recv_timeout(Duration::from_secs(2));
    controller.close().unwrap();
    let (read, bytes) = outcome.expect("closed full queue left its reader blocked");
    read.unwrap();
    assert_eq!(bytes, b"datadata");
    worker.join().unwrap();
}

#[test]
fn disconnected_client_releases_an_unacknowledged_open() {
    for buffered in [false, true] {
        let (mut controller, _, received, _) = fixture_with_timeout(None);
        let address = controller
            .forward("127.0.0.1:0".parse().unwrap(), "tcp:7000")
            .unwrap();
        let mut socket = TcpStream::connect(address).unwrap();
        let opened = received.recv_timeout(Duration::from_secs(2)).unwrap();
        let local = opened.header().arg0();
        if buffered {
            socket
                .write_all(b"queued before OPEN acknowledgment")
                .unwrap();
        }
        drop(socket);
        let deadline = Instant::now() + Duration::from_secs(2);
        while controller.hub.streams.lock().unwrap().contains_key(&local)
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(5));
        }
        let released = !controller.hub.streams.lock().unwrap().contains_key(&local);
        controller.close().unwrap();
        assert!(
            released,
            "disconnected client retained a pending stream reservation (buffered={buffered})"
        );
    }
}

#[test]
fn data_sent_before_open_acknowledgment_is_preserved() {
    let (mut controller, replies, received, _) = fixture_with_timeout(None);
    let address = controller
        .forward("127.0.0.1:0".parse().unwrap(), "tcp:7000")
        .unwrap();
    let mut socket = TcpStream::connect(address).unwrap();
    socket.write_all(b"early bytes").unwrap();
    let opened = received.recv_timeout(Duration::from_secs(2)).unwrap();
    let local = opened.header().arg0();
    replies.send(packet(Command::Okay, 42, local, &[])).unwrap();
    let message = received.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(message.header().command(), Command::Write);
    assert_eq!(message.payload(), b"early bytes");
    replies.send(packet(Command::Okay, 42, local, &[])).unwrap();
    controller.close().unwrap();
}

#[derive(Clone)]
struct BlockedTransport {
    inner: FakeTransport,
    blocked: Arc<(Mutex<bool>, Condvar)>,
    entered: mpsc::Sender<()>,
}
impl ADBTransport for BlockedTransport {
    fn connect(&mut self) -> Result<()> {
        self.inner.connect()
    }
    fn disconnect(&mut self) -> Result<()> {
        let (blocked, changed) = &*self.blocked;
        *blocked.lock().unwrap() = false;
        changed.notify_all();
        ADBTransport::disconnect(&mut self.inner)
    }
}
impl ADBMessageTransport for BlockedTransport {
    fn read_message_with_timeout(&mut self, timeout: Duration) -> Result<Message> {
        self.inner.read_message_with_timeout(timeout)
    }
    fn write_message_with_timeout(&mut self, message: Message, timeout: Duration) -> Result<()> {
        if message.header().command() == Command::Write {
            self.entered.send(()).unwrap();
            let (blocked, changed) = &*self.blocked;
            let mut blocked = blocked.lock().unwrap();
            while *blocked {
                blocked = changed.wait(blocked).unwrap();
            }
            return Err(broken().into());
        }
        self.inner.write_message_with_timeout(message, timeout)
    }
}

#[test]
fn shutdown_interrupts_a_blocked_writer_and_dispatcher() {
    let (replies, incoming) = mpsc::channel();
    let (outgoing, received) = mpsc::channel();
    let (entered, writing) = mpsc::channel();
    let transport = BlockedTransport {
        inner: FakeTransport {
            incoming: Arc::new(Mutex::new(incoming)),
            replies: replies.clone(),
            outgoing,
            disconnected: Arc::new(AtomicBool::new(false)),
        },
        blocked: Arc::new((Mutex::new(true), Condvar::new())),
        entered,
    };
    let mut emergency = transport.clone();
    let mut controller = ADBForwarder::new(
        ConnectionOwner(transport.inner.clone()),
        transport,
        4096,
        ADBTimeouts {
            write: None,
            ..ADBTimeouts::default()
        },
    );
    let address = controller
        .forward("127.0.0.1:0".parse().unwrap(), "tcp:7000")
        .unwrap();
    let mut socket = TcpStream::connect(address).unwrap();
    let opened = received.recv_timeout(Duration::from_secs(2)).unwrap();
    replies
        .send(packet(Command::Okay, 42, opened.header().arg0(), &[]))
        .unwrap();
    socket.write_all(b"blocked write").unwrap();
    writing.recv_timeout(Duration::from_secs(2)).unwrap();
    // The dispatcher tries to reject this unregistered incoming service while
    // the first worker owns the writer mutex indefinitely.
    replies
        .send(packet(Command::Open, 999, 0, b"tcp:1\0"))
        .unwrap();
    let (done, result) = mpsc::channel();
    let worker = thread::spawn(move || {
        done.send(controller.close()).unwrap();
    });
    let closed = result.recv_timeout(Duration::from_secs(2));
    // Also unblock the original implementation so a failed regression can end.
    ADBTransport::disconnect(&mut emergency).unwrap();
    worker.join().unwrap();
    closed
        .expect("shutdown waited behind the blocked writer")
        .unwrap();
}

#[test]
fn reverse_rules_share_targets_until_the_last_rule_is_removed() {
    let (mut controller, replies, received, disconnected) = fixture();
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = target.local_addr().unwrap();
    let service = format!("tcp:{}\0", address.port());
    let (opened_tx, opened_rx) = mpsc::channel();
    let peer = thread::spawn(move || {
        let mut controls = std::collections::HashSet::new();
        while !disconnected.load(Ordering::SeqCst) {
            let Ok(message) = received.recv_timeout(Duration::from_millis(50)) else {
                continue;
            };
            let local = message.header().arg0();
            let remote = message.header().arg1();
            match message.header().command() {
                Command::Open => {
                    controls.insert(local);
                    replies
                        .send(packet(Command::Okay, 100 + local, local, &[]))
                        .unwrap();
                    let response = if message.payload().starts_with(b"reverse:forward:tcp:7002;") {
                        b"FAILno listener available".as_slice()
                    } else {
                        b"OKAY".as_slice()
                    };
                    replies
                        .send(packet(Command::Write, 100 + local, local, response))
                        .unwrap();
                }
                Command::Okay if controls.contains(&local) => {
                    replies
                        .send(packet(Command::Clse, remote, local, &[]))
                        .unwrap();
                }
                Command::Okay if remote == 900 || remote == 901 => {
                    opened_tx.send(remote).unwrap();
                    replies
                        .send(packet(Command::Clse, remote, local, &[]))
                        .unwrap();
                }
                Command::Clse if remote == 902 => {
                    assert_eq!(local, 0);
                    opened_tx.send(remote).unwrap();
                }
                Command::Clse => {}
                other => panic!("unexpected {other}"),
            }
        }
    });
    assert_eq!(controller.reverse(7000, address).unwrap(), 7000);
    let second = controller.reverse(7001, address);
    if let Err(error) = second {
        controller.close().unwrap();
        peer.join().unwrap();
        panic!("a distinct reverse port could not share its target: {error}");
    }
    assert_eq!(second.unwrap(), 7001);
    assert!(controller.reverse(7002, address).is_err());
    // Failed additions must not remove an already shared target.
    controller
        .hub
        .route(packet(Command::Open, 900, 0, service.as_bytes()))
        .unwrap();
    assert_eq!(opened_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 900);
    controller.reverse_remove(7000).unwrap();
    controller
        .hub
        .route(packet(Command::Open, 901, 0, service.as_bytes()))
        .unwrap();
    assert_eq!(opened_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 901);
    controller.reverse_remove(7001).unwrap();
    controller
        .hub
        .route(packet(Command::Open, 902, 0, service.as_bytes()))
        .unwrap();
    assert_eq!(opened_rx.recv_timeout(Duration::from_secs(2)).unwrap(), 902);
    controller.close().unwrap();
    peer.join().unwrap();
}
