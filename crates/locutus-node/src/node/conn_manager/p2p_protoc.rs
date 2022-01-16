use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    net::IpAddr,
    pin::Pin,
    sync::Arc,
    task::Poll,
    time::{Duration, Instant},
};

use asynchronous_codec::{BytesMut, Framed};
use dashmap::{DashMap, DashSet};
use either::{Left, Right};
use futures::{
    future::{self},
    sink, stream, AsyncRead, AsyncWrite, FutureExt, Sink, SinkExt, Stream, StreamExt, TryStreamExt,
};
use libp2p::{
    core::{connection::ConnectionId, muxing, transport, ConnectedPoint, UpgradeInfo},
    identify::{self, IdentifyEvent, IdentifyInfo},
    identity::Keypair,
    multiaddr::Protocol,
    ping,
    swarm::{
        dial_opts::DialOpts, protocols_handler::OutboundUpgradeSend, AddressScore, KeepAlive,
        NegotiatedSubstream, NetworkBehaviour, NetworkBehaviourAction, NotifyHandler,
        ProtocolsHandler, ProtocolsHandlerEvent, ProtocolsHandlerUpgrErr, SubstreamProtocol,
        SwarmBuilder, SwarmEvent,
    },
    InboundUpgrade, Multiaddr, OutboundUpgrade, PeerId, Swarm,
};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use unsigned_varint::codec::UviBytes;

use crate::{
    config::{self, GlobalExecutor},
    message::{Message, NodeActions},
    node::{handle_cancelled_op, process_message, OpManager, PeerKey},
    ring::PeerKeyLocation,
    InitPeerNode, NodeConfig,
};

use super::{ConnectionBridge, ConnectionError};

/// The default maximum size for a varint length-delimited packet.
pub const DEFAULT_MAX_PACKET_SIZE: usize = 16 * 1024;

const CURRENT_AGENT_VER: &str = "/locutus/agent/0.1.0";
const CURRENT_PROTOC_VER: &[u8] = b"/locutus/0.1.0";
const CURRENT_PROTOC_VER_STR: &str = "/locutus/0.1.0";
const CURRENT_IDENTIFY_PROTOC_VER: &str = "/id/1.0.0";

fn config_behaviour(local_key: &Keypair, gateways: &[InitPeerNode]) -> NetBehaviour {
    let routing_table = gateways
        .iter()
        .filter_map(|p| {
            p.addr
                .as_ref()
                .map(|addr| (p.identifier, vec![addr.clone()]))
        })
        .collect();

    let ident_config =
        identify::IdentifyConfig::new(CURRENT_IDENTIFY_PROTOC_VER.to_string(), local_key.public())
            .with_agent_version(CURRENT_AGENT_VER.to_string());

    let ping = if cfg!(debug_assertions) {
        ping::Ping::new(ping::PingConfig::new().with_keep_alive(true))
    } else {
        ping::Ping::default()
    };
    NetBehaviour {
        identify: identify::Identify::new(ident_config),
        ping,
        locutus: LocutusBehaviour {
            queue: VecDeque::new(),
            routing_table,
            connected: HashMap::new(),
            openning_connection: HashSet::new(),
            pending: HashMap::new(),
        },
    }
}

/// Small helper function to convert a tuple composed of an IP address and a port
/// to a libp2p Multiaddr type.
fn multiaddr_from_connection(conn: (IpAddr, u16)) -> Multiaddr {
    let mut addr = Multiaddr::with_capacity(2);
    addr.push(Protocol::from(conn.0));
    addr.push(Protocol::Tcp(conn.1));
    addr
}

#[derive(Debug, Clone)]
struct P2pConnBridge {
    sender: Sender<(PeerKey, Box<Message>)>,
}

#[derive(Clone)]
pub(in crate::node) struct P2pBridge {
    active_net_connections: Arc<DashMap<PeerKey, Multiaddr>>,
    accepted_peers: Arc<DashSet<PeerKey>>,
    ev_listener_tx: Sender<(PeerKey, Box<Message>)>,
}

impl P2pBridge {
    fn new(sender: Sender<(PeerKey, Box<Message>)>) -> Self {
        Self {
            active_net_connections: Arc::new(DashMap::new()),
            accepted_peers: Arc::new(DashSet::new()),
            ev_listener_tx: sender,
        }
    }
}

#[async_trait::async_trait]
impl ConnectionBridge for P2pBridge {
    fn add_connection(&mut self, peer: PeerKey) -> super::ConnResult<()> {
        // todo: notify protocol ev listener thought the channel to accept this peer messages
        if self.active_net_connections.contains_key(&peer) {
            self.accepted_peers.insert(peer);
        }
        Ok(())
    }

    fn drop_connection(&mut self, peer: &PeerKey) {
        self.accepted_peers.remove(peer);
        // todo: notify protocol ev listener thought the channel to not accept this peer messages
    }

    async fn send(&self, target: &PeerKey, msg: Message) -> super::ConnResult<()> {
        self.ev_listener_tx
            .send((*target, Box::new(msg)))
            .await
            .map_err(|_| ConnectionError::SendNotCompleted)?;
        Ok(())
    }
}

pub(in crate::node) struct P2pConnManager {
    pub(in crate::node) swarm: Swarm<NetBehaviour>,
    conn_bridge_rx: Receiver<(PeerKey, Box<Message>)>,
    listen_on: Option<(IpAddr, u16)>,
    pub(in crate::node) gateways: Vec<PeerKeyLocation>,
    pub(in crate::node) bridge: P2pBridge,
}

impl P2pConnManager {
    pub fn build(
        transport: transport::Boxed<(PeerId, muxing::StreamMuxerBox)>,
        config: &NodeConfig,
    ) -> Result<Self, anyhow::Error> {
        // We set a global executor which is virtually the Tokio multi-threaded executor
        // to reuse it's thread pool and scheduler in order to drive futures.
        let global_executor = Box::new(GlobalExecutor);
        let builder = SwarmBuilder::new(
            transport,
            config_behaviour(&config.local_key, &config.remote_nodes),
            PeerId::from(config.local_key.public()),
        )
        .executor(global_executor);

        let mut swarm = builder.build();
        for remote_addr in config.remote_nodes.iter().filter_map(|r| r.addr.clone()) {
            swarm.add_external_address(remote_addr, AddressScore::Infinite);
        }

        let (tx_bridge_cmd, rx_bridge_cmd) = channel(100);
        let bridge = P2pBridge::new(tx_bridge_cmd);

        let gateways = config.get_gateways()?;
        Ok(P2pConnManager {
            swarm,
            conn_bridge_rx: rx_bridge_cmd,
            listen_on: config.local_ip.zip(config.local_port),
            gateways,
            bridge,
        })
    }

    pub fn listen_on(&mut self) -> Result<(), anyhow::Error> {
        if let Some(conn) = self.listen_on {
            let listening_addr = multiaddr_from_connection(conn);
            self.swarm.listen_on(listening_addr)?;
        }
        Ok(())
    }

    pub async fn run_event_listener<CErr>(
        mut self,
        op_manager: Arc<OpManager<CErr>>,
        mut notification_channel: Receiver<Message>,
    ) -> Result<(), anyhow::Error>
    where
        CErr: std::error::Error + Send + Sync + 'static,
    {
        use ConnMngrActions::*;

        loop {
            let net_msg = self.swarm.select_next_some().map(|event| match event {
                SwarmEvent::Behaviour(NetEvent::Locutus(msg)) => Ok(Left(msg)),
                SwarmEvent::ConnectionClosed { peer_id, .. } => {
                    Ok(Right(ConnMngrActions::ConnectionClosed {
                        peer_id: PeerKey::from(peer_id),
                    }))
                }
                SwarmEvent::Dialing(peer_id) => {
                    log::debug!("Attempting connection to {}", peer_id);
                    Ok(Right(ConnMngrActions::NoAction))
                }
                SwarmEvent::Behaviour(NetEvent::Identify(id)) => {
                    if let IdentifyEvent::Received { peer_id, info } = *id {
                        if Self::is_compatible_peer(&info) {
                            Ok(Right(ConnMngrActions::ConnectionEstablished {
                                peer_id: PeerKey(peer_id),
                                addr: info.observed_addr,
                            }))
                        } else {
                            log::warn!("Incompatible peer: {}, disconnecting", peer_id);
                            Ok(Right(ConnMngrActions::ConnectionClosed {
                                peer_id: PeerKey::from(peer_id),
                            }))
                        }
                    } else {
                        Ok(Right(ConnMngrActions::NoAction))
                    }
                }
                other_event => {
                    log::debug!("Received other swarm event: {:?}", other_event);
                    Ok(Right(ConnMngrActions::NoAction))
                }
            });

            let notification_msg = notification_channel.recv().map(|m| match m {
                Some(m) => Ok(Left(Box::new(m))),
                None => Ok(Right(ClosedChannel)),
            });

            let bridge_msg = self.conn_bridge_rx.recv().map(|msg| {
                if let Some((peer, msg)) = msg {
                    Ok(Right(SendMessage { peer, msg }))
                } else {
                    Ok(Right(ClosedChannel))
                }
            });

            let msg: Result<_, ConnectionError> = tokio::select! {
                msg = net_msg => { msg }
                msg = notification_msg => { msg }
                msg = bridge_msg => { msg }
            };

            match msg {
                Ok(Left(msg)) => {
                    log::debug!("Received swarm message at {}", op_manager.ring.peer_key);
                    let cb = self.bridge.clone();
                    match *msg {
                        Message::Canceled(tx) => {
                            handle_cancelled_op(
                                tx,
                                op_manager.ring.peer_key,
                                self.gateways.iter(),
                                &op_manager,
                                &mut self.bridge,
                            )
                            .await?;
                            continue;
                        }
                        Message::Internal(action) => {
                            log::debug!("internal action: {:?}", action);
                        }
                        msg => {
                            GlobalExecutor::spawn(process_message(
                                Ok(msg),
                                op_manager.clone(),
                                cb,
                                None,
                            ));
                        }
                    }
                }
                Ok(Right(ConnectionEstablished { addr, peer_id })) => {
                    log::debug!("Established connection with peer {} @ {}", peer_id, addr);
                    self.bridge.active_net_connections.insert(peer_id, addr);
                }
                Ok(Right(ConnectionClosed { peer_id })) => {
                    self.bridge.active_net_connections.remove(&peer_id);
                    // todo: notify the handler, read `disconnect_peer_id` doc
                    let _ = self.swarm.disconnect_peer_id(peer_id.0);
                    log::debug!("Dropped connection with peer {}", peer_id);
                }
                Ok(Right(SendMessage { peer, msg })) => {
                    log::debug!(
                        "Sending swarm message from {} to {}",
                        op_manager.ring.peer_key,
                        peer
                    );
                    self.swarm
                        .behaviour_mut()
                        .locutus
                        .queue
                        .push_front((peer.0, *msg));
                }
                Ok(Right(ClosedChannel)) => {
                    log::info!("Notification channel closed");
                    break;
                }
                Err(err) => {
                    let cb = self.bridge.clone();
                    GlobalExecutor::spawn(process_message(Err(err), op_manager.clone(), cb, None));
                }
                Ok(Right(NoAction)) => {}
            }
        }
        Ok(())
    }

    fn is_compatible_peer(info: &IdentifyInfo) -> bool {
        let compatible_agent = info.agent_version == CURRENT_AGENT_VER;
        let compatible_protoc = info
            .protocols
            .iter()
            .any(|s| s.as_str() == CURRENT_PROTOC_VER_STR);
        compatible_agent && compatible_protoc
    }
}

enum ConnMngrActions {
    /// Received a new connection
    ConnectionEstablished {
        peer_id: PeerKey,
        addr: Multiaddr,
    },
    /// Closed a connection with the peer
    ConnectionClosed {
        peer_id: PeerKey,
    },
    SendMessage {
        peer: PeerKey,
        msg: Box<Message>,
    },
    ClosedChannel,
    NoAction,
}

/// Manages network connections with different peers and event routing within the swarm.
pub(in crate::node) struct LocutusBehaviour {
    queue: VecDeque<(PeerId, Message)>,
    routing_table: HashMap<PeerId, Vec<Multiaddr>>,
    connected: HashMap<PeerId, ConnectionId>,
    openning_connection: HashSet<PeerId>,
    pending: HashMap<PeerId, Vec<Message>>,
}

impl NetworkBehaviour for LocutusBehaviour {
    type ProtocolsHandler = Handler;

    type OutEvent = Message;

    fn new_handler(&mut self) -> Self::ProtocolsHandler {
        Handler::new()
    }

    fn inject_connection_established(
        &mut self,
        peer_id: &PeerId,
        connection_id: &ConnectionId,
        endpoint: &ConnectedPoint,
        _failed_addresses: Option<&Vec<Multiaddr>>,
    ) {
        self.openning_connection.remove(peer_id);
        self.connected.insert(*peer_id, *connection_id);
        self.routing_table
            .entry(*peer_id)
            .or_default()
            .push(endpoint.get_remote_address().clone());
    }

    fn inject_event(&mut self, peer_id: PeerId, _connection: ConnectionId, msg: Message) {
        log::debug!("injected event in swarm: {:?}", msg);
        self.queue.push_front((peer_id, msg));
    }

    fn inject_disconnected(&mut self, peer: &PeerId) {
        self.connected.remove(peer);
    }

    fn poll(
        &mut self,
        _: &mut std::task::Context<'_>,
        _: &mut impl libp2p::swarm::PollParameters,
    ) -> std::task::Poll<NetworkBehaviourAction<Self::OutEvent, Self::ProtocolsHandler>> {
        if let Some((peer_id, msg)) = self.queue.pop_back() {
            if let Message::Internal(NodeActions::Error(err)) = msg {
                log::warn!("connection error: {}", err);
                return Poll::Pending;
            }

            if let Some(id) = self.connected.get(&peer_id) {
                let send_to_handler = NetworkBehaviourAction::NotifyHandler {
                    peer_id,
                    handler: NotifyHandler::One(*id),
                    event: msg,
                };
                Poll::Ready(send_to_handler)
            } else if self.openning_connection.contains(&peer_id) {
                // waiting to have an open connection
                self.queue.push_front((peer_id, msg));
                Poll::Pending
            } else if let Some(conn) = self.routing_table.get(&peer_id) {
                // initiate a connection if one does not exist
                let peer_opts = DialOpts::peer_id(peer_id)
                    .addresses(conn.clone())
                    .extend_addresses_through_behaviour();
                let initiate_conn = NetworkBehaviourAction::Dial {
                    opts: peer_opts.build(),
                    handler: self.new_handler(),
                };
                self.queue.push_front((peer_id, msg));
                self.openning_connection.insert(peer_id);
                Poll::Ready(initiate_conn)
            } else {
                log::error!("unknown addresses for peer {}", peer_id);
                Poll::Pending
            }
        } else {
            Poll::Pending
        }
    }
}

type UniqConnId = usize;

/// Handles the connection with a given peer.
pub(in crate::node) struct Handler {
    substreams: Vec<SubstreamState>,
    keep_alive: KeepAlive,
    uniq_conn_id: UniqConnId,
    protocol_status: ProtocolStatus,
    pending: Vec<Message>,
}

enum ProtocolStatus {
    Unconfirmed,
    Confirmed,
    Reported,
    FailedUpgrade,
}

enum SubstreamState {
    /// We haven't started opening the outgoing substream yet.
    /// Contains the initial request we want to send.
    OutPendingOpen { msg: Message, conn_id: UniqConnId },
    /// Waiting for the first message after requesting an outbound open connection.
    AwaitingFirst { conn_id: UniqConnId },
    FreeStream {
        conn_id: UniqConnId,
        substream: LocutusStream<NegotiatedSubstream>,
    },
    /// Waiting to send a message to the remote.
    PendingSend {
        conn_id: UniqConnId,
        substream: LocutusStream<NegotiatedSubstream>,
        msg: Box<Message>,
    },
    /// Waiting to flush the substream so that the data arrives to the remote.
    PendingFlush {
        conn_id: UniqConnId,
        substream: LocutusStream<NegotiatedSubstream>,
    },
    /// Waiting for an answer back from the remote.
    WaitingMsg {
        conn_id: UniqConnId,
        substream: LocutusStream<NegotiatedSubstream>,
    },
    /// An error happened on the substream and we should report the error to the user.
    ReportError {
        conn_id: UniqConnId,
        error: ConnectionError,
    },
}

impl SubstreamState {
    fn is_free(&self) -> bool {
        matches!(self, SubstreamState::FreeStream { .. })
    }
}

impl Handler {
    const KEEP_ALIVE: Duration = Duration::from_secs(30);

    fn new() -> Self {
        Self {
            substreams: vec![],
            keep_alive: KeepAlive::Until(Instant::now() + config::PEER_TIMEOUT),
            uniq_conn_id: 0,
            protocol_status: ProtocolStatus::Unconfirmed,
            pending: Vec::new(),
        }
    }

    fn send_to_free_substream(&mut self, msg: Message) -> Option<Message> {
        let pos = self
            .substreams
            .iter()
            .position(|state| matches!(state, SubstreamState::FreeStream { .. }));

        if let Some(pos) = pos {
            let (conn_id, substream) = match self.substreams.swap_remove(pos) {
                SubstreamState::FreeStream {
                    substream: stream,
                    conn_id,
                } => (conn_id, stream),
                _ => unreachable!(),
            };

            self.substreams.push(SubstreamState::PendingSend {
                msg: Box::new(msg),
                conn_id,
                substream,
            });
            None
        } else {
            Some(msg)
        }
    }
}

type HandlePollingEv = ProtocolsHandlerEvent<LocutusProtocol, (), Message, ConnectionError>;

impl ProtocolsHandler for Handler {
    /// Event received from the network by the handler
    type InEvent = Message;

    /// Event producer by the handler and processed by the swarm
    type OutEvent = Message;

    type Error = ConnectionError;

    type InboundProtocol = LocutusProtocol;

    type OutboundProtocol = LocutusProtocol;

    type InboundOpenInfo = ();

    type OutboundOpenInfo = ();

    fn listen_protocol(&self) -> SubstreamProtocol<Self::InboundProtocol, Self::InboundOpenInfo> {
        SubstreamProtocol::new(LocutusProtocol, ())
    }

    fn inject_fully_negotiated_outbound(
        &mut self,
        stream: <Self::OutboundProtocol as InboundUpgrade<NegotiatedSubstream>>::Output,
        _info: Self::OutboundOpenInfo,
    ) {
        if let Some(pos) = self
            .substreams
            .iter()
            .position(|state| matches!(state, SubstreamState::AwaitingFirst { .. }))
        {
            let conn_id = match self.substreams.swap_remove(pos) {
                SubstreamState::AwaitingFirst { conn_id } => conn_id,
                _ => unreachable!(),
            };
            self.substreams.push(SubstreamState::WaitingMsg {
                conn_id,
                substream: stream,
            });
        } else {
            panic!()
        }
    }

    fn inject_fully_negotiated_inbound(
        &mut self,
        stream: <Self::OutboundProtocol as OutboundUpgrade<NegotiatedSubstream>>::Output,
        _info: Self::InboundOpenInfo,
    ) {
        if !self.substreams.iter().any(|state| {
            matches!(
                state,
                SubstreamState::AwaitingFirst { .. } | SubstreamState::WaitingMsg { .. }
            )
        }) {
            self.substreams.push(SubstreamState::WaitingMsg {
                conn_id: self.uniq_conn_id,
                substream: stream,
            });
            self.uniq_conn_id += 1;
        }
        if let ProtocolStatus::Unconfirmed = self.protocol_status {
            self.protocol_status = ProtocolStatus::Confirmed;
        }
    }

    fn inject_event(&mut self, msg: Self::InEvent) {
        log::debug!(
            "inject event (dest: {:?}):\n  {:?}",
            msg.target().map(|pkl| pkl.peer),
            msg
        );
        if let Some(msg) = self.send_to_free_substream(msg) {
            let conn_id = self.uniq_conn_id;
            self.uniq_conn_id += 1;
            // is the first request initiated and/or there are no free substreams, open a new one
            self.substreams
                .push(SubstreamState::OutPendingOpen { msg, conn_id });
        }
    }

    fn inject_dial_upgrade_error(
        &mut self,
        _: Self::OutboundOpenInfo,
        error: ProtocolsHandlerUpgrErr<<Self::OutboundProtocol as OutboundUpgradeSend>::Error>,
    ) {
        self.protocol_status = ProtocolStatus::FailedUpgrade;
        self.substreams.push(SubstreamState::ReportError {
            conn_id: self.uniq_conn_id,
            error: (Box::new(error)).into(),
        });
        self.uniq_conn_id += 1;
    }

    fn connection_keep_alive(&self) -> KeepAlive {
        // fixme self.keep_alive
        KeepAlive::Yes
    }

    fn poll(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<HandlePollingEv> {
        if self.substreams.is_empty() {
            return Poll::Pending;
        }

        if let ProtocolStatus::Confirmed = self.protocol_status {
            self.protocol_status = ProtocolStatus::Reported;
            return Poll::Ready(ProtocolsHandlerEvent::Custom(Message::Internal(
                NodeActions::ConfirmedInbound,
            )));
        }

        for n in (0..self.substreams.len()).rev() {
            let mut stream = self.substreams.swap_remove(n);
            loop {
                match stream {
                    SubstreamState::OutPendingOpen { msg, conn_id } => {
                        let event = ProtocolsHandlerEvent::OutboundSubstreamRequest {
                            protocol: SubstreamProtocol::new(LocutusProtocol, ()),
                        };
                        self.substreams
                            .push(SubstreamState::AwaitingFirst { conn_id });
                        self.pending.push(msg);
                        if self.substreams.is_empty() {
                            self.keep_alive =
                                KeepAlive::Until(Instant::now() + config::PEER_TIMEOUT);
                        }
                        return Poll::Ready(event);
                    }
                    SubstreamState::AwaitingFirst { conn_id } => {
                        self.substreams
                            .push(SubstreamState::AwaitingFirst { conn_id });
                        break;
                    }
                    SubstreamState::FreeStream { substream, conn_id } => {
                        if let Some(msg) = self.pending.pop() {
                            stream = SubstreamState::PendingSend {
                                substream,
                                conn_id,
                                msg: Box::new(msg),
                            };
                            continue;
                        } else {
                            self.substreams
                                .push(SubstreamState::FreeStream { substream, conn_id });
                            break;
                        }
                    }
                    SubstreamState::PendingSend {
                        mut substream,
                        msg,
                        conn_id,
                    } => match Sink::poll_ready(Pin::new(&mut substream), cx) {
                        Poll::Ready(Ok(())) => {
                            if let Message::Internal(action) = *msg {
                                match action {
                                    NodeActions::ConfirmedInbound => {
                                        log::debug!("waiting to send back msg (id: {})", conn_id);
                                        stream = SubstreamState::FreeStream { substream, conn_id };
                                        continue;
                                    }
                                    NodeActions::ShutdownNode | NodeActions::Error(_) => break,
                                }
                            }
                            log::debug!("sending (id: {})", conn_id);
                            match Sink::start_send(Pin::new(&mut substream), *msg) {
                                Ok(()) => {
                                    log::debug!("sent (id: {})", conn_id);
                                    stream = SubstreamState::PendingFlush { substream, conn_id };
                                }
                                Err(err) => {
                                    let event = ProtocolsHandlerEvent::Custom(Message::Internal(
                                        NodeActions::Error(err),
                                    ));
                                    return Poll::Ready(event);
                                }
                            }
                        }
                        Poll::Pending => {
                            stream = SubstreamState::PendingSend {
                                substream,
                                msg,
                                conn_id,
                            };
                            continue;
                        }
                        Poll::Ready(Err(err)) => {
                            let event = ProtocolsHandlerEvent::Custom(Message::Internal(
                                NodeActions::Error(err),
                            ));
                            return Poll::Ready(event);
                        }
                    },
                    SubstreamState::PendingFlush {
                        mut substream,
                        conn_id,
                    } => match Sink::poll_flush(Pin::new(&mut substream), cx) {
                        Poll::Ready(Ok(())) => {
                            log::debug!("flushed (id: {})", conn_id);
                            stream = SubstreamState::WaitingMsg { substream, conn_id };
                            continue;
                        }
                        Poll::Pending => {
                            self.substreams
                                .push(SubstreamState::PendingFlush { substream, conn_id });
                            break;
                        }
                        Poll::Ready(Err(err)) => {
                            let event = ProtocolsHandlerEvent::Custom(Message::Internal(
                                NodeActions::Error(err),
                            ));
                            return Poll::Ready(event);
                        }
                    },
                    SubstreamState::WaitingMsg {
                        mut substream,
                        conn_id,
                    } => match Stream::poll_next(Pin::new(&mut substream), cx) {
                        Poll::Ready(Some(Ok(msg))) => {
                            log::debug!("received (id: {}) msg: {:?}", conn_id, msg);
                            if !msg.terminal() {
                                // received a message, the other peer is waiting for an answer
                                log::debug!("waiting to send back more (id: {})", conn_id);
                                self.substreams
                                    .push(SubstreamState::FreeStream { substream, conn_id });
                            }
                            let event = ProtocolsHandlerEvent::Custom(msg);
                            return Poll::Ready(event);
                        }
                        Poll::Pending => {
                            self.substreams
                                .push(SubstreamState::WaitingMsg { substream, conn_id });
                            break;
                        }
                        Poll::Ready(Some(Err(err))) => {
                            let event = ProtocolsHandlerEvent::Custom(Message::Internal(
                                NodeActions::Error(err),
                            ));
                            return Poll::Ready(event);
                        }
                        Poll::Ready(None) => {
                            let event = ProtocolsHandlerEvent::Custom(Message::Internal(
                                NodeActions::Error(ConnectionError::IOError(Some(
                                    io::ErrorKind::UnexpectedEof.into(),
                                ))),
                            ));
                            return Poll::Ready(event);
                        }
                    },
                    SubstreamState::ReportError { error, .. } => {
                        let event = ProtocolsHandlerEvent::Custom(Message::Internal(
                            NodeActions::Error(error),
                        ));
                        return Poll::Ready(event);
                    }
                }
            }
        }

        if self.substreams.is_empty() || self.substreams.iter().all(|s| s.is_free()) {
            // We destroyed all substreams in this iteration or all substreams are free
            self.keep_alive = KeepAlive::Until(Instant::now() + config::PEER_TIMEOUT);
        } else {
            self.keep_alive = KeepAlive::Yes;
        }

        Poll::Pending
    }
}

pub(crate) struct LocutusProtocol;

impl UpgradeInfo for LocutusProtocol {
    type Info = &'static [u8];
    type InfoIter = std::iter::Once<Self::Info>;

    fn protocol_info(&self) -> Self::InfoIter {
        std::iter::once(CURRENT_PROTOC_VER)
    }
}

pub(crate) type LocutusStream<S> = stream::AndThen<
    sink::With<
        stream::ErrInto<Framed<S, UviBytes<io::Cursor<Vec<u8>>>>, ConnectionError>,
        io::Cursor<Vec<u8>>,
        Message,
        future::Ready<Result<io::Cursor<Vec<u8>>, ConnectionError>>,
        fn(Message) -> future::Ready<Result<io::Cursor<Vec<u8>>, ConnectionError>>,
    >,
    future::Ready<Result<Message, ConnectionError>>,
    fn(BytesMut) -> future::Ready<Result<Message, ConnectionError>>,
>;

impl<S> InboundUpgrade<S> for LocutusProtocol
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    type Output = LocutusStream<S>;
    type Error = ConnectionError;
    type Future = future::Ready<Result<Self::Output, Self::Error>>;

    fn upgrade_inbound(self, incoming: S, _: Self::Info) -> Self::Future {
        frame_stream(incoming)
    }
}

impl<S> OutboundUpgrade<S> for LocutusProtocol
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    type Output = LocutusStream<S>;
    type Error = ConnectionError;
    type Future = future::Ready<Result<Self::Output, Self::Error>>;

    fn upgrade_outbound(self, incoming: S, _: Self::Info) -> Self::Future {
        frame_stream(incoming)
    }
}

fn frame_stream<S>(incoming: S) -> future::Ready<Result<LocutusStream<S>, ConnectionError>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut codec = UviBytes::default();
    codec.set_max_len(DEFAULT_MAX_PACKET_SIZE);
    let framed = Framed::new(incoming, codec)
        .err_into()
        .with::<_, _, fn(_) -> _, _>(|response| match encode_msg(response) {
            Ok(msg) => future::ready(Ok(io::Cursor::new(msg))),
            Err(err) => future::ready(Err(err)),
        })
        .and_then::<_, fn(_) -> _>(|bytes| future::ready(decode_msg(bytes)));
    future::ok(framed)
}

#[inline(always)]
fn encode_msg(msg: Message) -> Result<Vec<u8>, ConnectionError> {
    bincode::serialize(&msg).map_err(|err| ConnectionError::Serialization(Some(err)))
}

#[inline(always)]
fn decode_msg(buf: BytesMut) -> Result<Message, ConnectionError> {
    let cursor = std::io::Cursor::new(buf);
    bincode::deserialize_from(cursor).map_err(|err| ConnectionError::Serialization(Some(err)))
}

/// The network behaviour implements the following capabilities:
///
/// - [Identify](https://github.com/libp2p/specs/tree/master/identify) libp2p protocol.
/// - Pinging between peers.
#[derive(libp2p::NetworkBehaviour)]
#[behaviour(event_process = false)]
#[behaviour(out_event = "NetEvent")]
pub(in crate::node) struct NetBehaviour {
    identify: identify::Identify,
    ping: ping::Ping,
    locutus: LocutusBehaviour,
}

#[derive(Debug)]
pub(in crate::node) enum NetEvent {
    Locutus(Box<Message>),
    Identify(Box<identify::IdentifyEvent>),
    Ping(ping::PingEvent),
}

impl From<identify::IdentifyEvent> for NetEvent {
    fn from(event: identify::IdentifyEvent) -> NetEvent {
        Self::Identify(Box::new(event))
    }
}

impl From<ping::PingEvent> for NetEvent {
    fn from(event: ping::PingEvent) -> NetEvent {
        Self::Ping(event)
    }
}

impl From<Message> for NetEvent {
    fn from(event: Message) -> NetEvent {
        Self::Locutus(Box::new(event))
    }
}
