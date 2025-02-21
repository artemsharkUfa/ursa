//! # Ursa Behaviour implementation.
//!
//! Ursa custom behaviour implements [`NetworkBehaviour`] with the following options:
//!
//! - [`Ping`] A `NetworkBehaviour` that responds to inbound pings and
//!   periodically sends outbound pings on every established connection.
//! - [`Identify`] A `NetworkBehaviour` that automatically identifies nodes periodically, returns information
//!   about them, and answers identify queries from other nodes.
//! - [`Bitswap`] A `NetworkBehaviour` that handles sending and receiving blocks.
//! - [`Gossipsub`] A `NetworkBehaviour` that handles the gossipsub protocol.
//! - [`DiscoveryBehaviour`]
//! - [`RequestResponse`] A `NetworkBehaviour` that implements a generic
//!   request/response protocol or protocol family, whereby each request is
//!   sent over a new substream on a connection.

use anyhow::{Error, Result};
use cid::Cid;
use fnv::FnvHashMap;
use futures::channel::oneshot;
use libipld::store::StoreParams;
use libp2p::autonat::{Event, NatStatus};
use libp2p::dcutr;
use libp2p::ping::PingConfig;
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::{
    autonat::{Behaviour as Autonat, Config as AutonatConfig, Event as AutonatEvent},
    dcutr::behaviour::Event as DcutrEvent,
    gossipsub::{
        error::{PublishError, SubscriptionError},
        Gossipsub, GossipsubEvent, GossipsubMessage, IdentTopic as Topic, MessageId,
        PeerScoreParams, PeerScoreThresholds, TopicHash,
    },
    identify::{Identify, IdentifyConfig, IdentifyEvent},
    identity::Keypair,
    kad,
    ping::{Ping, PingEvent, PingFailure, PingSuccess},
    relay::v2::{
        client::{Client as RelayClient, Event as RelayClientEvent},
        relay::{Config as RelayConfig, Event as RelayServerEvent, Relay as RelayServer},
    },
    request_response::{
        ProtocolSupport, RequestId, RequestResponse, RequestResponseConfig, RequestResponseEvent,
        RequestResponseMessage, ResponseChannel,
    },
    swarm::{
        NetworkBehaviour, NetworkBehaviourAction, NetworkBehaviourEventProcess, PollParameters,
    },
    Multiaddr, NetworkBehaviour, PeerId,
};
use libp2p_bitswap::{Bitswap, BitswapConfig, BitswapEvent, BitswapStore, QueryId};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    iter,
    task::{Context, Poll},
    time::Duration,
};
use tracing::{debug, error, trace, warn};
use ursa_utils::convert_cid;

use crate::discovery::URSA_KAD_PROTOCOL;
use crate::{
    codec::protocol::{UrsaExchangeCodec, UrsaExchangeRequest, UrsaExchangeResponse, UrsaProtocol},
    config::NetworkConfig,
    discovery::{DiscoveryBehaviour, DiscoveryEvent},
    gossipsub::UrsaGossipsub,
};

pub type BlockSenderChannel<T> = oneshot::Sender<Result<T, Error>>;

#[derive(Debug)]
pub struct BitswapInfo {
    pub cid: Cid,
    pub query_id: QueryId,
    pub block_found: bool,
}

pub const IPFS_PROTOCOL: &str = "ipfs/0.1.0";

fn ursa_agent() -> String {
    format!("ursa/{}", env!("CARGO_PKG_VERSION"))
}

/// [Behaviour]'s events
/// Requests and failure events emitted by the `NetworkBehaviour`.
#[derive(Debug)]
pub enum BehaviourEvent {
    NatStatusChanged {
        old: NatStatus,
        new: NatStatus,
    },
    /// An event trigger when remote peer connects.
    PeerConnected(PeerId),
    /// An event trigger when remote peer disconnects.
    PeerDisconnected(PeerId),
    /// An event trigger when relay reservation is opened
    RelayReservationOpened {
        peer_id: PeerId,
    },
    /// An event trigger when relay reservation is closed
    RelayReservationClosed {
        peer_id: PeerId,
    },
    /// An event trigger when a relay circuit is opened
    RelayCircuitOpened,
    /// An event trigger when a relay circuit is closed
    RelayCircuitClosed,
    /// A Gossip message request was received from a peer.
    Bitswap(BitswapInfo),
    GossipMessage {
        peer: PeerId,
        topic: TopicHash,
        message: GossipsubMessage,
    },
    /// A message request was received from a peer.
    /// Attached is a channel for returning a response.
    RequestMessage {
        peer: PeerId,
        request: UrsaExchangeRequest,
        channel: ResponseChannel<UrsaExchangeResponse>,
    },
    StartPublish {
        public_address: Multiaddr,
    },
}

/// A `Networkbehaviour` that handles Ursa's different protocol implementations.
///
/// The poll function must have the same signature as the NetworkBehaviour
/// function and will be called last within the generated NetworkBehaviour implementation.
///
/// The events generated [`BehaviourEvent`].
#[derive(NetworkBehaviour)]
#[behaviour(
    out_event = "BehaviourEvent",
    poll_method = "poll",
    event_process = true
)]
pub struct Behaviour<P: StoreParams> {
    /// Alive checks.
    ping: Ping,

    // Identify and exchange info with other peers.
    identify: Identify,

    /// autonat
    autonat: Toggle<Autonat>,

    /// Relay client. Used to listen on a relay for incoming connections.
    relay_client: Toggle<RelayClient>,

    /// Relay server. Used to allow other peers to route through the node
    relay_server: Toggle<RelayServer>,

    /// DCUtR
    dcutr: Toggle<dcutr::behaviour::Behaviour>,

    /// Bitswap for exchanging data between blocks between peers.
    bitswap: Bitswap<P>,

    /// Ursa's gossiping protocol for message propagation.
    gossipsub: Gossipsub,

    /// Kademlia discovery and bootstrap.
    discovery: DiscoveryBehaviour,

    /// request/response protocol implementation for [`UrsaProtocol`]
    request_response: RequestResponse<UrsaExchangeCodec>,

    /// Ursa's emitted events.
    #[behaviour(ignore)]
    events: VecDeque<BehaviourEvent>,

    /// Pending requests
    #[behaviour(ignore)]
    pending_requests: HashMap<RequestId, ResponseChannel<UrsaExchangeResponse>>,

    /// Pending responses
    #[behaviour(ignore)]
    pending_responses: HashMap<RequestId, oneshot::Sender<Result<UrsaExchangeResponse>>>,

    #[behaviour(ignore)]
    queries: FnvHashMap<QueryId, BitswapInfo>,
}

impl<P: StoreParams> Behaviour<P> {
    pub fn new<S: BitswapStore<Params = P>>(
        keypair: &Keypair,
        config: &NetworkConfig,
        bitswap_store: S,
        relay_client: Option<libp2p::relay::v2::client::Client>,
    ) -> Self {
        let local_public_key = keypair.public();
        let local_peer_id = PeerId::from(local_public_key.clone());

        // Setup the ping behaviour
        let ping = Ping::new(PingConfig::new().with_keep_alive(true));

        // Setup the gossip behaviour
        let mut gossipsub = UrsaGossipsub::new(keypair, config);
        // todo(botch): handle gracefully
        gossipsub
            .with_peer_score(PeerScoreParams::default(), PeerScoreThresholds::default())
            .expect("PeerScoreParams and PeerScoreThresholds");

        // Setup the discovery behaviour
        let discovery = DiscoveryBehaviour::new(keypair, config);

        // Setup the bitswap behaviour
        let bitswap = Bitswap::new(BitswapConfig::default(), bitswap_store);

        // Setup the identify behaviour
        let identify = Identify::new(
            IdentifyConfig::new(IPFS_PROTOCOL.into(), keypair.public())
                .with_agent_version(ursa_agent()),
        );

        let request_response = {
            let mut cfg = RequestResponseConfig::default();

            // todo(botch): calculate an upper limit to allow for large files
            cfg.set_request_timeout(Duration::from_secs(60));

            let protocols = iter::once((UrsaProtocol, ProtocolSupport::Full));

            RequestResponse::new(UrsaExchangeCodec, protocols, cfg)
        };

        let autonat = config
            .autonat
            .then(|| {
                let config = AutonatConfig {
                    throttle_server_period: Duration::from_secs(30),
                    ..AutonatConfig::default()
                };

                Autonat::new(local_peer_id, config)
            })
            .into();

        let relay_server = config
            .relay_server
            .then(|| RelayServer::new(local_public_key.into(), RelayConfig::default()))
            .into();

        let dcutr = config
            .relay_client
            .then(|| {
                if relay_client.is_none() {
                    panic!("relay client not instantiated");
                }
                dcutr::behaviour::Behaviour::new()
            })
            .into();

        Behaviour {
            ping,
            autonat,
            relay_server,
            relay_client: relay_client.into(),
            dcutr,
            bitswap,
            identify,
            gossipsub,
            discovery,
            request_response,
            events: VecDeque::new(),
            pending_requests: HashMap::default(),
            pending_responses: HashMap::default(),
            queries: Default::default(),
        }
    }

    pub fn publish(
        &mut self,
        topic: Topic,
        data: GossipsubMessage,
    ) -> Result<MessageId, PublishError> {
        self.gossipsub.publish(topic, data.data)
    }

    pub fn public_address(&self) -> Option<&Multiaddr> {
        self.autonat.as_ref().and_then(|a| a.public_address())
    }

    pub fn peers(&self) -> HashSet<PeerId> {
        self.discovery.peers().clone()
    }

    pub fn is_relay_client_enabled(&self) -> bool {
        self.relay_client.is_enabled()
    }

    pub fn discovery(&mut self) -> &mut DiscoveryBehaviour {
        &mut self.discovery
    }

    pub fn bootstrap(&mut self) -> Result<kad::QueryId, Error> {
        self.discovery.bootstrap()
    }

    pub fn subscribe(&mut self, topic: &Topic) -> Result<bool, SubscriptionError> {
        self.gossipsub.subscribe(topic)
    }

    pub fn unsubscribe(&mut self, topic: &Topic) -> Result<bool, PublishError> {
        self.gossipsub.unsubscribe(topic)
    }

    pub fn publish_ad(&mut self, public_address: Multiaddr) -> Result<()> {
        self.events
            .push_back(BehaviourEvent::StartPublish { public_address });
        Ok(())
    }

    pub fn send_request(
        &mut self,
        peer: PeerId,
        request: UrsaExchangeRequest,
        sender: oneshot::Sender<Result<UrsaExchangeResponse>>,
    ) -> Result<()> {
        let request_id = self.request_response.send_request(&peer, request);
        self.pending_responses.insert(request_id, sender);

        Ok(())
    }

    pub fn get_block(&mut self, cid: Cid, providers: impl Iterator<Item = PeerId>) {
        debug!("get block via rpc called, the requested cid is: {:?}", cid);
        let id = self.bitswap.get(convert_cid(cid.to_bytes()), providers);

        self.queries.insert(
            id,
            BitswapInfo {
                query_id: id,
                cid,
                block_found: false,
            },
        );
    }

    pub fn sync_block(&mut self, cid: Cid, providers: Vec<PeerId>) {
        debug!(
            "sync block via http called, the requested root cid is: {:?}",
            cid
        );
        let c_cid = convert_cid(cid.to_bytes());
        let id = self.bitswap.sync(c_cid, providers, std::iter::once(c_cid));
        self.queries.insert(
            id,
            BitswapInfo {
                query_id: id,
                cid,
                block_found: false,
            },
        );
    }

    pub fn cancel(&mut self, id: QueryId) {
        self.queries.remove(&id);
        self.bitswap.cancel(id);
    }

    fn poll(
        &mut self,
        _: &mut Context,
        _: &mut impl PollParameters,
    ) -> Poll<
        NetworkBehaviourAction<
            <Self as NetworkBehaviour>::OutEvent,
            <Self as NetworkBehaviour>::ConnectionHandler,
        >,
    > {
        if let Some(event) = self.events.pop_front() {
            return Poll::Ready(NetworkBehaviourAction::GenerateEvent(event));
        }

        Poll::Pending
    }

    fn handle_ping(&mut self, event: PingEvent) {
        let peer = event.peer.to_base58();

        match event.result {
            Ok(result) => match result {
                PingSuccess::Pong => {
                    trace!(
                        "PingSuccess::Pong] - received a ping and sent back a pong to {}",
                        peer
                    );
                }
                PingSuccess::Ping { rtt } => {
                    trace!(
                        "[PingSuccess::Ping] - with rtt {} from {} in ms",
                        rtt.as_millis(),
                        peer
                    );
                    // perhaps we can set rtt for each peer
                }
            },
            Err(err) => {
                match err {
                    PingFailure::Timeout => {
                        debug!(
                            "[PingFailure::Timeout] - no response was received from {}",
                            peer
                        );
                        // remove peer from list of connected.
                    }
                    PingFailure::Unsupported => {
                        debug!("[PingFailure::Unsupported] - the peer {} does not support the ping protocol", peer);
                    }
                    PingFailure::Other { error } => {
                        debug!(
                            "[PingFailure::Other] - the ping failed with {} for reasons {}",
                            peer, error
                        );
                    }
                }
            }
        }
    }

    fn handle_identify(&mut self, event: IdentifyEvent) {
        debug!("[IdentifyEvent] {:?}", event);
        match event {
            IdentifyEvent::Received { peer_id, info } => {
                trace!(
                    "[IdentifyEvent::Received] - with version {} has been received from a peer {}.",
                    info.protocol_version,
                    peer_id
                );

                if self.peers().contains(&peer_id) {
                    trace!(
                        "[IdentifyEvent::Received] - peer {} already known!",
                        peer_id
                    );
                }

                // check if received identify is from a peer on the same network
                if info
                    .protocols
                    .iter()
                    .any(|name| name.as_bytes() == URSA_KAD_PROTOCOL)
                {
                    self.gossipsub.add_explicit_peer(&peer_id);

                    for address in info.listen_addrs {
                        self.discovery.add_address(&peer_id, address.clone());
                        self.request_response.add_address(&peer_id, address.clone());
                    }
                }
            }
            IdentifyEvent::Sent { .. }
            | IdentifyEvent::Pushed { .. }
            | IdentifyEvent::Error { .. } => {}
        }
    }

    fn handle_autonat(&mut self, event: AutonatEvent) {
        debug!("[AutonatEvent] {:?}", event);
        match event {
            AutonatEvent::StatusChanged { old, new } => {
                self.events
                    .push_back(BehaviourEvent::NatStatusChanged { old, new });
            }
            Event::OutboundProbe(_) | Event::InboundProbe(_) => {}
        }
    }

    fn handle_relay_server(&mut self, event: RelayServerEvent) {
        debug!("[RelayServerEvent] {:?}", event);

        match event {
            RelayServerEvent::ReservationReqAccepted {
                src_peer_id,
                renewed,
            } => {
                if !renewed {
                    self.events
                        .push_back(BehaviourEvent::RelayReservationOpened {
                            peer_id: src_peer_id,
                        });
                }
            }
            RelayServerEvent::ReservationTimedOut { src_peer_id } => {
                self.events
                    .push_back(BehaviourEvent::RelayReservationClosed {
                        peer_id: src_peer_id,
                    });
            }
            RelayServerEvent::CircuitReqAccepted { .. } => {
                self.events.push_back(BehaviourEvent::RelayCircuitOpened);
            }
            RelayServerEvent::CircuitClosed { .. } => {
                self.events.push_back(BehaviourEvent::RelayCircuitClosed);
            }
            _ => {}
        }
    }

    fn handle_relay_client(&mut self, event: RelayClientEvent) {
        debug!("[RelayClientEvent] {:?}", event);
    }

    fn handle_dcutr(&mut self, event: DcutrEvent) {
        debug!("[DcutrEvent] {:?}", event);
    }

    fn handle_bitswap(&mut self, event: BitswapEvent) {
        match event {
            BitswapEvent::Progress(id, missing) => {
                debug!(
                    "progress in bitswap sync query, id: {}, missing: {}",
                    id, missing
                );
            }
            BitswapEvent::Complete(id, result) => {
                debug!(
                    "[BitswapEvent::Complete] - Bitswap Event complete for query id: {:?}",
                    id
                );
                match self.queries.remove(&id) {
                    Some(mut info) => {
                        match result {
                            Err(err) => error!("{:?}", err),
                            Ok(_res) => info.block_found = true,
                        }
                        self.events.push_back(BehaviourEvent::Bitswap(info));
                    }
                    _ => {
                        error!(
                            "[BitswapEvent::Complete] - Query Id {:?} not found in the hash map",
                            id
                        )
                    }
                }
            }
        }
    }

    fn handle_gossipsub(&mut self, event: GossipsubEvent) {
        match event {
            GossipsubEvent::Message {
                propagation_source,
                message,
                ..
            } => {
                self.events.push_back(BehaviourEvent::GossipMessage {
                    peer: propagation_source,
                    topic: message.topic.clone(),
                    message,
                });
            }
            GossipsubEvent::Subscribed { peer_id, topic } => {
                // A remote subscribed to a topic.
                // subscribe to new topic.
            }
            GossipsubEvent::Unsubscribed { peer_id, topic } => {
                // A remote unsubscribed from a topic.
                // remove subscription.
            }
            GossipsubEvent::GossipsubNotSupported { peer_id } => {
                // A peer that does not support gossipsub has connected.
                // the scoring/rating should happen here.
                // disconnect.
            }
        }
    }

    fn handle_discovery(&mut self, event: DiscoveryEvent) {
        match event {
            DiscoveryEvent::Connected(peer_id) => {
                self.events
                    .push_back(BehaviourEvent::PeerConnected(peer_id));
            }
            DiscoveryEvent::Disconnected(peer_id) => {
                self.events
                    .push_back(BehaviourEvent::PeerDisconnected(peer_id));
            }
        }
    }

    fn handle_request_response(
        &mut self,
        event: RequestResponseEvent<UrsaExchangeRequest, UrsaExchangeResponse>,
    ) {
        match event {
            RequestResponseEvent::Message { peer, message } => {
                match message {
                    RequestResponseMessage::Request {
                        request_id,
                        request,
                        channel,
                    } => {
                        debug!(
                            "[RequestResponseMessage::Request] - {} {}: {:?}",
                            request_id, peer, request
                        );
                        // self.pending_requests.insert(request_id, channel);

                        self.events.push_back(BehaviourEvent::RequestMessage {
                            peer,
                            request,
                            channel,
                        });
                    }
                    RequestResponseMessage::Response {
                        request_id,
                        response,
                    } => {
                        debug!(
                            "[RequestResponseMessage::Response] - {} {}: {:?}",
                            request_id, peer, response
                        );

                        if let Some(request) = self.pending_responses.remove(&request_id) {
                            if request.send(Ok(response)).is_err() {
                                warn!("[RequestResponseMessage::Response] - failed to send request: {:?}", request_id);
                            }
                        }

                        debug!("[RequestResponseMessage::Response] - failed to remove channel for: {:?}", request_id);
                    }
                }
            }
            RequestResponseEvent::OutboundFailure {
                peer,
                request_id,
                error,
            } => {
                debug!(
                    "[RequestResponseMessage::OutboundFailure] - {} {}: {:?}",
                    peer.to_string(),
                    request_id.to_string(),
                    error.to_string()
                );

                if let Some(request) = self.pending_responses.remove(&request_id) {
                    if request.send(Err(error.into())).is_err() {
                        warn!("[RequestResponseMessage::OutboundFailure] - failed to send request: {:?}", request_id);
                    }
                }

                debug!("[RequestResponseMessage::OutboundFailure] - failed to remove channel for: {:?}", request_id);
            }
            RequestResponseEvent::InboundFailure {
                peer,
                request_id,
                error,
            } => {
                warn!(
                    "[RequestResponseMessage::InboundFailure] - {} {}: {:?}",
                    peer.to_string(),
                    request_id.to_string(),
                    error.to_string()
                );
            }
            RequestResponseEvent::ResponseSent { peer, request_id } => {
                debug!(
                    "[RequestResponseMessage::ResponseSent] - {}: {}",
                    peer.to_string(),
                    request_id.to_string(),
                );
            }
        }
    }
}

impl<P: StoreParams> NetworkBehaviourEventProcess<PingEvent> for Behaviour<P> {
    fn inject_event(&mut self, event: PingEvent) {
        self.handle_ping(event)
    }
}

impl<P: StoreParams> NetworkBehaviourEventProcess<IdentifyEvent> for Behaviour<P> {
    fn inject_event(&mut self, event: IdentifyEvent) {
        self.handle_identify(event)
    }
}

impl<P: StoreParams> NetworkBehaviourEventProcess<GossipsubEvent> for Behaviour<P> {
    fn inject_event(&mut self, event: GossipsubEvent) {
        self.handle_gossipsub(event)
    }
}

impl<P: StoreParams> NetworkBehaviourEventProcess<BitswapEvent> for Behaviour<P> {
    fn inject_event(&mut self, event: BitswapEvent) {
        self.handle_bitswap(event)
    }
}

impl<P: StoreParams> NetworkBehaviourEventProcess<DiscoveryEvent> for Behaviour<P> {
    fn inject_event(&mut self, event: DiscoveryEvent) {
        self.handle_discovery(event)
    }
}

impl<P: StoreParams> NetworkBehaviourEventProcess<AutonatEvent> for Behaviour<P> {
    fn inject_event(&mut self, event: AutonatEvent) {
        self.handle_autonat(event)
    }
}

impl<P: StoreParams> NetworkBehaviourEventProcess<RelayServerEvent> for Behaviour<P> {
    fn inject_event(&mut self, event: RelayServerEvent) {
        self.handle_relay_server(event)
    }
}

impl<P: StoreParams> NetworkBehaviourEventProcess<RelayClientEvent> for Behaviour<P> {
    fn inject_event(&mut self, event: RelayClientEvent) {
        self.handle_relay_client(event)
    }
}

impl<P: StoreParams> NetworkBehaviourEventProcess<DcutrEvent> for Behaviour<P> {
    fn inject_event(&mut self, event: DcutrEvent) {
        self.handle_dcutr(event)
    }
}

impl<P: StoreParams>
    NetworkBehaviourEventProcess<RequestResponseEvent<UrsaExchangeRequest, UrsaExchangeResponse>>
    for Behaviour<P>
{
    fn inject_event(
        &mut self,
        event: RequestResponseEvent<UrsaExchangeRequest, UrsaExchangeResponse>,
    ) {
        self.handle_request_response(event)
    }
}
