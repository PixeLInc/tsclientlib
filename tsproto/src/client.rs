use std::cell::RefCell;
use std::mem;
use std::net::SocketAddr;
use std::rc::Rc;

use {tomcrypt, base64};
use chrono::Utc;
use futures::{self, future, Future, Sink, Stream};
use futures::future::Either;
use futures::task::{self, Task};
use futures::unsync::oneshot;
use num::{pow, BigUint, Integer, ToPrimitive};
use rand::{self, Rng};

use {packets, BoxFuture, Error, Result};
use algorithms as algs;
use commands::Command;
use handler_data::*;
use handler_data::Data;
use packets::*;
use resend::ResendFuture;

/// The data of our client.
pub type ClientData = Data<ServerConnectionData>;
/// Connection to a server from our client.
pub(crate) type ServerConnection = Connection<ServerConnectionData>;

pub struct ServerConnectionData {
    pub state_change_listener: Vec<Box<FnMut() -> BoxFuture<(), Error>>>,
    pub state: ServerConnectionState,
}

#[derive(Debug)]
pub enum ServerConnectionState {
    /// After `Init0` was sent.
    Init0 { version: u32, random0: [u8; 4] },
    /// After `Init2` was sent.
    Init2 { version: u32 },
    /// After `Init4` was sent.
    ClientInitIv { alpha: [u8; 10] },
    /// The initial handshake is done and the next packet has to be
    /// `clientinit`.
    Connecting,
    /// Fully connected, the client id is known.
    Connected,
    /// The connection is finished, no more packets can be sent or received.
    Disconnected,
}

fn create_init_header() -> Header {
    let mut mac = [0; 8];
    mac.copy_from_slice(b"TS3INIT1");
    let mut header = Header {
        mac,
        p_id: 0x65,
        c_id: Some(0),
        p_type: 0,
    };
    header.set_type(PacketType::Init);
    header.set_unencrypted(true);
    header
}

/// Configures the default setup chain, including logging and decoding
/// of packets.
pub fn default_setup(data: Rc<RefCell<ClientData>>) {
    // Packet encoding
    ::packet_codec::PacketCodecSink::apply(data.clone());
    ::packet_codec::PacketCodecStream::apply(data.clone(), true);
    // Logging
    ::log::apply_udp_packet_logger(data.clone());
    ::log::apply_packet_logger(data.clone());

    // Default handlers
    DefaultPacketHandler::apply(data.clone(), true);

    // Resend packets
    let resend_future = ResendFuture::new(
        data.clone(),
        Box::new(Data::get_udp_packets(data.clone())),
    );
    let (handle, logger) = {
        let data = data.borrow();
        (data.handle.clone(), data.logger.clone())
    };
    handle.spawn(resend_future.map_err(move |e| {
        error!(logger, "Resend"; "error" => ?e);
    }));
}

/// Wait until a client reaches a certain state.
///
/// `is_state` should return `true`, if the state is reached and `false` if this
/// function should continue waiting.
pub fn wait_for_state<F: Fn(&ServerConnectionState) -> bool + 'static>(
    data: Rc<RefCell<ClientData>>,
    server_addr: SocketAddr,
    f: F,
) -> BoxFuture<(), Error> {
    // Return a future that resolves when we are connected
    let data2 = data.clone();
    if let Some(con) = data.borrow_mut().connections.get_mut(&server_addr) {
        if f(&con.state.state) {
            return Box::new(future::ok(()));
        }
        // Wait for the next state change
        let (send, recv) = oneshot::channel();
        let mut send = Some(send);
        con.state.state_change_listener.push(Box::new(move || {
            send.take().unwrap().send(()).unwrap();
            Box::new(future::ok(()))
        }));
        Box::new(
            recv.map_err(|e| e.into())
                .and_then(move |_| wait_for_state(data2, server_addr, f)),
        )
    } else {
        Box::new(future::ok(()))
    }
}

pub fn wait_until_connected(
    data: Rc<RefCell<ClientData>>,
    server_addr: SocketAddr,
) -> BoxFuture<(), Error> {
    wait_for_state(data, server_addr, |state| {
        if let ServerConnectionState::Connected = *state {
            true
        } else {
            false
        }
    })
}

/// Connect to a server.
///
/// This function returns, when the client reached the
/// [`ServerConnectionState::Connecting`] state. Then the client should send the
/// `clientinit` packet and call [`wait_until_connected`].
///
/// [`ServerConnectionState::Connecting`]:
/// [`wait_until_connected`]:
pub fn connect(
    data: Rc<RefCell<ClientData>>,
    server_addr: SocketAddr,
) -> BoxFuture<(), Error> {
    // Send the first init packet
    // Get the current timestamp
    let now = Utc::now();
    let timestamp = now.timestamp() as u32;
    let mut rng = rand::thread_rng();

    // Random bytes
    let random0 = rng.gen::<[u8; 4]>();
    let packet_data = C2SInit::Init0 {
        version: timestamp,
        timestamp: timestamp,
        random0,
    };

    let cheader = create_init_header();
    // Add the connection to the connection list
    {
        let mut data = data.borrow_mut();
        data.connections.insert(
            server_addr,
            ServerConnection::new(ServerConnectionData {
                state_change_listener: Vec::new(),
                state: ServerConnectionState::Init0 {
                    version: timestamp,
                    random0,
                },
            }),
        );
    }

    let packet = Packet::new(cheader, packets::Data::C2SInit(packet_data));
    Box::new(
        ClientData::get_packets(data.clone())
            .send((server_addr, packet))
            .and_then(move |_| {
                wait_for_state(data, server_addr, |state| {
                    if let ServerConnectionState::Connecting = *state {
                        true
                    } else {
                        false
                    }
                })
            }),
    )
}

pub struct DefaultPacketHandlerStream {
    inner_stream: Box<Stream<Item = (SocketAddr, Packet), Error = Error>>,
}

impl DefaultPacketHandlerStream {
    pub fn new<
        InnerStream: Stream<Item = (SocketAddr, Packet), Error = Error> + 'static,
        InnerSink: Sink<SinkItem = (SocketAddr, Packet), SinkError = Error> + 'static,
    >(
        data: Rc<RefCell<ClientData>>,
        inner_stream: InnerStream,
        inner_sink: InnerSink,
        send_clientinit: bool,
    ) -> (Self, Rc<RefCell<Either<InnerSink, Option<Task>>>>) {
        let sink = Rc::new(RefCell::new(Either::A(inner_sink)));
        let sink2 = sink.clone();
        let data = Rc::downgrade(&data);
        let inner_stream = Box::new(inner_stream.and_then(move |(addr, packet)| -> BoxFuture<_, _> {
            // true, if the packet should not be handled further.
            let mut ignore_packet = false;
            // If the connection should be removed
            let mut is_end = false;
            // Check if we have a connection for this server
            let packet_res = {
                let data = data.upgrade().unwrap();
                let mut data = data.borrow_mut();
                let logger = data.logger.clone();
                let data = &mut *data;
                if let Some(con) = data.connections.get_mut(&addr) {
                    let mut update_rtt = None;
                    let handle_res = match con.state.state {
                        ServerConnectionState::Init0 { version, ref random0 } => {
                            // Handle an Init1
                            if let Packet { data: packets::Data::S2CInit(
                                S2CInit::Init1 { ref random1, ref random0_r }), .. } = packet {
                                // Check the response
                                if random0.as_ref().iter().rev().eq(random0_r.as_ref()) {
                                    // The packet is correct.
                                    // Send next init packet
                                    let cheader = create_init_header();
                                    let data = C2SInit::Init2 {
                                        version: version,
                                        random1: *random1,
                                        random0_r: *random0_r,
                                    };

                                    let state = ServerConnectionState::Init2 {
                                        version,
                                    };

                                    ignore_packet = true;
                                    Some((state, Some(Packet::new(cheader,
                                        packets::Data::C2SInit(data)))))
                                } else {
                                    error!(logger, "Init: Got wrong data in the \
                                        Init1 response packet");
                                    None
                                }
                            } else {
                                None
                            }
                        }
                        ServerConnectionState::Init2 { version } => {
                            // Handle an Init3
                            if let Packet { data: packets::Data::S2CInit(
                                S2CInit::Init3 { ref x, ref n, level, ref random2 }), .. } = packet {
                                // Solve RSA puzzle: y = x ^ (2 ^ level) % n
                                // Use Montgomery Reduction
                                let xi = BigUint::from_bytes_be(x);
                                let ni = BigUint::from_bytes_be(n);
                                // TODO implement Montgomery Reduction, use another thread + timeout after 5s
                                fn pow_mod(mut x: BigUint, level: u32, n: &BigUint) -> BigUint {
                                    for _ in 0..level {
                                        x = pow::pow(x, 2).mod_floor(n);
                                    }
                                    x
                                }
                                let mut time_reporter = ::slog_perf::TimeReporter::new_with_level(
                                    "Solve RSA puzzle", logger.clone(),
                                    ::slog::Level::Info);
                                time_reporter.start("");
                                let yi = pow_mod(xi.clone(), level, &ni);
                                time_reporter.finish();
                                info!(logger, "Solve RSA puzzle";
                                      "level" => level, "x" => %xi, "n" => %ni,
                                      "y" => %yi);
                                let y = algs::biguint_to_array(&yi);

                                let omega = tryf!(data.private_key.export_public());

                                // Create the command string
                                let mut rng = rand::thread_rng();
                                let alpha = rng.gen::<[u8; 10]>();
                                // omega is an ASN.1-DER encoded public key from the
                                // ECDH parameters.
                                let alpha_s = base64::encode(&alpha);
                                let omega_s = base64::encode(&omega);
                                let mut command = Command::new("clientinitiv");
                                command.push("alpha", alpha_s);
                                command.push("omega", omega_s);
                                command.push("ot", "1");
                                command.push("ip", "");

                                let cheader = create_init_header();
                                let data = C2SInit::Init4 {
                                    version,
                                    x: *x,
                                    n: *n,
                                    level,
                                    random2: *random2,
                                    y,
                                    command: command.clone(),
                                };

                                let state = ServerConnectionState::ClientInitIv {
                                    alpha,
                                };

                                ignore_packet = true;
                                Some((state, Some(Packet::new(cheader,
                                    packets::Data::C2SInit(data)))))
                            } else {
                                None
                            }
                        }
                        ServerConnectionState::ClientInitIv { ref alpha } => {
                            let private_key = &mut data.private_key;
                            let res = (|con_params: &mut Option<ConnectedParams>| -> Result<()> {
                                if let Packet { data: packets::Data::Command(ref command), .. } = packet {
                                    let cmd = command.get_commands().remove(0);
                                    if cmd.command != "initivexpand"
                                        || !cmd.has_arg("alpha")
                                        || !cmd.has_arg("beta")
                                        || !cmd.has_arg("omega")
                                        || base64::decode(cmd.args["alpha"])
                                        .map(|a| a != alpha).unwrap_or(true) {
                                        bail!("initivexpand command has wrong arguments");
                                    } else {
                                        let beta_vec = base64::decode(cmd.args["beta"])?;
                                        if beta_vec.len() != 10 {
                                            bail!("Incorrect beta length");
                                        }
                                        let omega = base64::decode(cmd.args["omega"])?;
                                        let mut beta = [0; 10];
                                        beta.copy_from_slice(&beta_vec);
                                        let mut server_key = tomcrypt::EccKey::import(&omega)?;

                                        let (iv, mac) = algs::compute_iv_mac(
                                            alpha, &beta, private_key, &mut server_key)?;
                                        let mut params = ConnectedParams::new(
                                            server_key, iv, mac);
                                        // We already sent a command packet.
                                        params.outgoing_p_ids[PacketType::Command.to_usize().unwrap()]
                                            .1 = 1;
                                        // We received a command packet.
                                        params.incoming_p_ids[PacketType::Command.to_usize().unwrap()]
                                            .1 = 1;
                                        // And we sent an ack.
                                        params.incoming_p_ids[PacketType::Ack.to_usize().unwrap()]
                                            .1 = 1;
                                        *con_params = Some(params);
                                    }
                                    Ok(())
                                } else {
                                    Ok(())
                                }})(&mut con.params);
                            if let Err(error) = res {
                                error!(logger, "Handle udp init packet"; "error" => ?error);
                                None
                            } else {
                                ignore_packet = true;
                                Some((ServerConnectionState::Connecting, None))
                            }
                        }
                        ServerConnectionState::Connecting => {
                            let mut res = None;
                            if let Packet { data: packets::Data::Command(ref cmd), .. } = packet {
                                let cmd = cmd.get_commands().remove(0);
                                if cmd.command == "initserver" && cmd.has_arg("aclid") {
                                    // Handle an initserver
                                    if let Some(ref mut params) = con.params {
                                        if let Ok(c_id) = cmd.args["aclid"].parse() {
                                            params.c_id = c_id;
                                        }
                                    }
                                    // initserver is the ack for clientinit
                                    // Remove from send queue
                                    let p_type = PacketType::Command;
                                    let p_id = 1;
                                    let mut rec = None;
                                    let mut items = data.send_queue.drain()
                                        .filter_map(|r| if r.p_type == p_type && r.p_id == p_id {
                                            rec = Some(r);
                                            None
                                        } else {
                                            Some(r)
                                        })
                                        .collect();
                                    mem::swap(&mut items, &mut data.send_queue);
                                    // Update smoothed round trip time
                                    if let Some(rec) = rec {
                                        // Only if it was not resent
                                        if rec.tries == 1 {
                                            let now = Utc::now();
                                            let diff = now.naive_utc().signed_duration_since(rec.sent.naive_utc());
                                            update_rtt = Some(diff);
                                        }
                                    }
                                    res = Some((ServerConnectionState::Connected, None));
                                }
                            }
                            res
                        }
                        ServerConnectionState::Connected => {
                            let mut res = None;
                            if let Packet { data: packets::Data::Command(ref cmd), .. } = packet {
                                let cmd = cmd.get_commands().remove(0);
                                if cmd.command == "notifyclientleftview" && cmd.has_arg("clid") {
                                    // Handle a disconnect
                                    if let Some(ref mut params) = con.params {
                                        if cmd.args["clid"].parse() == Ok(params.c_id) {
                                            is_end = true;
                                            res = Some((ServerConnectionState::Disconnected, None));
                                        }
                                    }
                                }
                            }
                            res
                        }
                        ServerConnectionState::Disconnected => {
                            warn!(logger, "Got packet from server after disconnecting");
                            return Box::new(future::ok(None));
                        }
                    };
                    if let Some((state, packet)) = handle_res {
                        if let Some(rtt) = update_rtt {
                            con.update_srtt(rtt);
                        }
                        con.state.state = state;
                        let listeners = mem::replace(&mut con.state.state_change_listener, Vec::new());
                        Some((listeners, packet))
                    } else {
                        None
                    }
                } else {
                    None
                }
            };

            if is_end {
                // Remove the connection
                let data = data.upgrade().unwrap();
                let mut data = data.borrow_mut();
                data.connections.remove(&addr);
            }

            if let Some((mut listeners, p)) = packet_res {
                // Notify state changed listeners
                let l_fut = future::join_all(listeners.drain(..).map(|mut l| l()).collect::<Vec<_>>());

                if let Some(p) = p {
                    if let packets::Data::Command(ref cmd) = p.data {
                        if cmd.command == "clientinit" {
                            if !send_clientinit {
                                return Box::new(l_fut.and_then(move |_| future::ok(None)));
                            }
                        }
                    }
                    // Take sink
                    let tmp_sink = mem::replace(&mut *sink.borrow_mut(), Either::B(None));
                    let tmp_sink = if let Either::A(sink) = tmp_sink {
                        sink
                    } else {
                        unreachable!("Sink is not available");
                    };
                    // Send the packet
                    let sink = sink.clone();
                    Box::new(l_fut.and_then(move |_| tmp_sink.send((addr, p)).map(move |tmp_sink| {
                        let s: Either<InnerSink, Option<Task>> = mem::replace(&mut *sink.borrow_mut(), Either::A(tmp_sink));
                        if let Either::B(Some(task)) = s {
                            // Notify the task, that the sink is available
                            task.notify();
                        }
                        // Already handled
                        if ignore_packet {
                            None
                        } else {
                            Some((addr, packet))
                        }
                    })))
                } else {
                    Box::new(l_fut.and_then(move |_| if ignore_packet {
                        future::ok(None)
                    } else {
                        future::ok(Some((addr, packet)))
                    }))
                }
            } else {
                if ignore_packet {
                    Box::new(future::ok(None))
                } else {
                    Box::new(future::ok(Some((addr, packet))))
                }
            }
        })
        .filter_map(|p| p));
        (Self { inner_stream }, sink2)
    }
}

impl Stream for DefaultPacketHandlerStream {
    type Item = (SocketAddr, Packet);
    type Error = Error;

    fn poll(&mut self) -> futures::Poll<Option<Self::Item>, Self::Error> {
        self.inner_stream.poll()
    }
}

pub struct DefaultPacketHandlerSink<
    InnerSink: Sink<SinkItem = (SocketAddr, Packet), SinkError = Error> + 'static,
> {
    inner_sink: Rc<RefCell<Either<InnerSink, Option<Task>>>>,
}

impl<
    InnerSink: Sink<SinkItem = (SocketAddr, Packet), SinkError = Error> + 'static,
> DefaultPacketHandlerSink<InnerSink> {
    pub fn new(
        inner_sink: Rc<RefCell<Either<InnerSink, Option<Task>>>>,
    ) -> Self {
        Self { inner_sink }
    }
}

impl<
    InnerSink: Sink<SinkItem = (SocketAddr, Packet), SinkError = Error> + 'static,
> Sink for DefaultPacketHandlerSink<InnerSink> {
    type SinkItem = InnerSink::SinkItem;
    type SinkError = InnerSink::SinkError;

    fn start_send(
        &mut self,
        item: Self::SinkItem,
    ) -> futures::StartSend<Self::SinkItem, Self::SinkError> {
        let mut sink = self.inner_sink.borrow_mut();
        if let Either::A(ref mut sink) = *sink {
            return sink.start_send(item);
        }
        *sink = Either::B(Some(task::current()));
        Ok(futures::AsyncSink::NotReady(item))
    }

    fn poll_complete(&mut self) -> futures::Poll<(), Self::SinkError> {
        let mut sink = self.inner_sink.borrow_mut();
        if let Either::A(ref mut sink) = *sink {
            return sink.poll_complete();
        }
        *sink = Either::B(Some(task::current()));
        Ok(futures::Async::NotReady)
    }

    fn close(&mut self) -> futures::Poll<(), Self::SinkError> {
        let mut sink = self.inner_sink.borrow_mut();
        if let Either::A(ref mut sink) = *sink {
            return sink.close();
        }
        *sink = Either::B(Some(task::current()));
        Ok(futures::Async::NotReady)
    }
}

pub struct DefaultPacketHandler<
    InnerSink: Sink<SinkItem = (SocketAddr, Packet), SinkError = Error> + 'static,
> {
    inner_stream: DefaultPacketHandlerStream,
    inner_sink: DefaultPacketHandlerSink<InnerSink>,
}

impl<
    InnerSink: Sink<SinkItem = (SocketAddr, Packet), SinkError = Error> + 'static,
> DefaultPacketHandler<InnerSink> {
    pub fn new<
        InnerStream: Stream<Item = (SocketAddr, Packet), Error = Error> + 'static,
    >(
        data: Rc<RefCell<ClientData>>,
        inner_stream: InnerStream,
        inner_sink: InnerSink,
        send_clientinit: bool,
    ) -> Self {
        let (inner_stream, inner_sink) = DefaultPacketHandlerStream::new(
            data,
            inner_stream,
            inner_sink,
            send_clientinit,
        );
        let inner_sink = DefaultPacketHandlerSink::new(inner_sink);
        Self {
            inner_stream,
            inner_sink,
        }
    }

    pub fn split(
        self,
    ) -> (
        DefaultPacketHandlerSink<InnerSink>,
        DefaultPacketHandlerStream,
    ) {
        (self.inner_sink, self.inner_stream)
    }
}

impl
    DefaultPacketHandler<
        Box<Sink<SinkItem = (SocketAddr, Packet), SinkError = Error>>,
    > {
    pub fn apply(data: Rc<RefCell<ClientData>>, send_clientinit: bool) {
        let (stream, sink) = {
            let mut data = data.borrow_mut();
            (
                data.packet_stream.take().unwrap(),
                data.packet_sink.take().unwrap(),
            )
        };
        let handler = Self::new(data.clone(), stream, sink, send_clientinit);
        let (sink, stream) = handler.split();
        let mut data = data.borrow_mut();
        data.packet_stream = Some(Box::new(stream));
        data.packet_sink = Some(Box::new(sink));
    }
}
