// Copyright (C) 2019-2021 Aleo Systems Inc.
// This file is part of the snarkOS library.

// The snarkOS library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkOS library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkOS library. If not, see <https://www.gnu.org/licenses/>.

use crate::{Environment, LedgerRequest, LedgerRouter, Message, NodeType};
use snarkvm::dpc::prelude::*;

use anyhow::{anyhow, Result};
use futures::SinkExt;
use rand::{prelude::IteratorRandom, rngs::OsRng, thread_rng, Rng};
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    time::{Duration, Instant, SystemTime},
};
use tokio::{net::TcpStream, sync::mpsc, task, time::timeout};
use tokio_stream::StreamExt;
use tokio_util::codec::Framed;

/// Shorthand for the parent half of the `Peer` outbound message channel.
pub(crate) type OutboundRouter<N, E> = mpsc::Sender<Message<N, E>>;
/// Shorthand for the child half of the `Peer` outbound message channel.
type OutboundHandler<N, E> = mpsc::Receiver<Message<N, E>>;

/// Shorthand for the parent half of the `Peers` message channel.
pub(crate) type PeersRouter<N, E> = mpsc::Sender<PeersRequest<N, E>>;
#[allow(unused)]
/// Shorthand for the child half of the `Peers` message channel.
type PeersHandler<N, E> = mpsc::Receiver<PeersRequest<N, E>>;

///
/// An enum of requests that the `Peers` struct processes.
///
#[derive(Debug)]
pub enum PeersRequest<N: Network, E: Environment> {
    /// Connect := (peer_ip, ledger_router)
    Connect(SocketAddr, LedgerRouter<N, E>),
    /// Heartbeat := (ledger_router)
    Heartbeat(LedgerRouter<N, E>),
    /// MessagePropagate := (peer_ip, message)
    MessagePropagate(SocketAddr, Message<N, E>),
    /// MessageSend := (peer_ip, message)
    MessageSend(SocketAddr, Message<N, E>),
    /// PeerConnecting := (stream, peer_ip, ledger_router)
    PeerConnecting(TcpStream, SocketAddr, LedgerRouter<N, E>),
    /// PeerConnected := (peer_ip, peer_nonce, outbound_router)
    PeerConnected(SocketAddr, u64, OutboundRouter<N, E>),
    /// PeerDisconnected := (peer_ip)
    PeerDisconnected(SocketAddr),
    /// PeerRestricted := (peer_ip)
    PeerRestricted(SocketAddr),
    /// SendPeerResponse := (peer_ip)
    SendPeerResponse(SocketAddr),
    /// ReceivePeerResponse := (\[peer_ip\])
    ReceivePeerResponse(Vec<SocketAddr>),
}

///
/// A list of peers connected to the node server.
///
pub struct Peers<N: Network, E: Environment> {
    /// The local address of this node.
    local_ip: SocketAddr,
    /// The local nonce for this node session.
    local_nonce: u64,
    /// The map connected peer IPs to their nonce and outbound message router.
    connected_peers: HashMap<SocketAddr, (u64, OutboundRouter<N, E>)>,
    /// The set of candidate peer IPs.
    candidate_peers: HashSet<SocketAddr>,
    /// The set of restricted peer IPs.
    restricted_peers: HashMap<SocketAddr, Instant>,
    /// The map of peers to their first-seen port number, number of attempts, and timestamp of the last inbound connection request.
    seen_inbound_connections: HashMap<SocketAddr, ((u16, u32), SystemTime)>,
    /// The map of peers to a map of block hashes to their last seen timestamp.
    seen_outbound_blocks: HashMap<SocketAddr, HashMap<N::BlockHash, SystemTime>>,
    /// The map of peers to the timestamp of their last outbound connection request.
    seen_outbound_connections: HashMap<SocketAddr, SystemTime>,
    /// The map of peers to a map of transaction IDs to their last seen timestamp.
    seen_outbound_transactions: HashMap<SocketAddr, HashMap<N::TransactionID, SystemTime>>,
}

impl<N: Network, E: Environment> Peers<N, E> {
    ///
    /// Initializes a new instance of `Peers`.
    ///
    pub(crate) fn new(local_ip: SocketAddr, local_nonce: Option<u64>) -> Self {
        let local_nonce = match local_nonce {
            Some(nonce) => nonce,
            None => thread_rng().gen(),
        };

        Self {
            local_ip,
            local_nonce,
            connected_peers: Default::default(),
            candidate_peers: Default::default(),
            restricted_peers: Default::default(),
            seen_outbound_blocks: Default::default(),
            seen_inbound_connections: Default::default(),
            seen_outbound_connections: Default::default(),
            seen_outbound_transactions: Default::default(),
        }
    }

    ///
    /// Returns `true` if the node is connected to the given IP.
    ///
    pub(crate) fn is_connected_to(&self, ip: SocketAddr) -> bool {
        self.connected_peers.contains_key(&ip)
    }

    ///
    /// Returns `true` if the given IP is restricted.
    ///
    pub(crate) fn is_restricted(&self, ip: SocketAddr) -> bool {
        match self.restricted_peers.get(&ip) {
            Some(timestamp) => timestamp.elapsed().as_secs() < E::RADIO_SILENCE_IN_SECS,
            None => false,
        }
    }

    ///
    /// Returns the list of connected peers.
    ///
    pub(crate) fn connected_peers(&self) -> Vec<SocketAddr> {
        self.connected_peers.keys().cloned().collect()
    }

    ///
    /// Returns the list of nonces for the connected peers.
    ///
    pub(crate) fn connected_nonces(&self) -> impl Iterator<Item = &u64> + '_ {
        self.connected_peers.values().map(|(peer_nonce, _)| peer_nonce)
    }

    ///
    /// Returns the list of candidate peers.
    ///
    pub(crate) fn candidate_peers(&self) -> &HashSet<SocketAddr> {
        &self.candidate_peers
    }

    ///
    /// Returns the number of connected peers.
    ///
    pub(crate) fn num_connected_peers(&self) -> usize {
        self.connected_peers.len()
    }

    ///
    /// Returns the number of candidate peers.
    ///
    pub(crate) fn num_candidate_peers(&self) -> usize {
        self.candidate_peers.len()
    }

    ///
    /// Performs the given `request` to the peers.
    /// All requests must go through this `update`, so that a unified view is preserved.
    ///
    pub(super) async fn update(&mut self, request: PeersRequest<N, E>, peers_router: &PeersRouter<N, E>) {
        match request {
            PeersRequest::Connect(peer_ip, ledger_router) => {
                // Ensure the peer IP is not this node.
                if peer_ip == self.local_ip
                    || (peer_ip.ip().is_unspecified() || peer_ip.ip().is_loopback()) && peer_ip.port() == self.local_ip.port()
                {
                    debug!("Skipping connection request to {} (attempted to self-connect)", peer_ip);
                }
                // Ensure the node does not surpass the maximum number of peer connections.
                else if self.num_connected_peers() >= E::MAXIMUM_NUMBER_OF_PEERS {
                    debug!("Skipping connection request to {} (maximum peers reached)", peer_ip);
                }
                // Ensure the peer is a new connection.
                else if self.is_connected_to(peer_ip) {
                    debug!("Skipping connection request to {} (already connected)", peer_ip);
                }
                // Ensure the peer is not restricted.
                else if self.is_restricted(peer_ip) {
                    debug!("Skipping connection request to {} (restricted)", peer_ip);
                }
                // Attempt to open a TCP stream.
                else {
                    // Ensure the node respects the connection frequency limit.
                    let last_seen = self.seen_outbound_connections.entry(peer_ip).or_insert(SystemTime::UNIX_EPOCH);
                    let elapsed = last_seen.elapsed().unwrap_or(Duration::MAX).as_secs();
                    if elapsed < E::RADIO_SILENCE_IN_SECS {
                        trace!("Skipping connection request to {} (tried {} secs ago)", peer_ip, elapsed);
                    } else {
                        debug!("Connecting to {}...", peer_ip);
                        // Update the last seen timestamp for this peer.
                        *last_seen = SystemTime::now();
                        // Initialize the peer handler.
                        match timeout(Duration::from_secs(E::CONNECTION_TIMEOUT_IN_SECS), TcpStream::connect(peer_ip)).await {
                            Ok(stream) => match stream {
                                Ok(stream) => {
                                    Peer::handler(
                                        stream,
                                        self.local_ip,
                                        self.local_nonce,
                                        peers_router,
                                        ledger_router,
                                        &mut self.connected_nonces(),
                                    )
                                    .await
                                }
                                Err(error) => {
                                    trace!("Failed to connect to '{}': '{:?}'", peer_ip, error);
                                    self.candidate_peers.remove(&peer_ip);
                                }
                            },
                            Err(error) => {
                                error!("Unable to reach '{}': '{:?}'", peer_ip, error);
                                self.candidate_peers.remove(&peer_ip);
                            }
                        };
                    }
                }
            }
            PeersRequest::Heartbeat(ledger_router) => {
                // Ensure the number of connected peers is below the maximum threshold.
                if self.num_connected_peers() > E::MAXIMUM_NUMBER_OF_PEERS {
                    debug!("Exceeded maximum number of connected peers");

                    // Determine the peers to disconnect from.
                    let num_excess_peers = self.num_connected_peers() - E::MAXIMUM_NUMBER_OF_PEERS;
                    let peer_ips_to_disconnect = self
                        .connected_peers
                        .iter()
                        .filter(|(&peer_ip, _)| {
                            let peer_str = peer_ip.to_string();
                            !E::SYNC_NODES.contains(&peer_str.as_str()) && !E::PEER_NODES.contains(&peer_str.as_str())
                        })
                        .take(num_excess_peers)
                        .map(|(&peer_ip, _)| peer_ip)
                        .collect::<Vec<SocketAddr>>();

                    // Proceed to send disconnect requests to these peers.
                    for peer_ip in peer_ips_to_disconnect {
                        info!("Disconnecting from {} (exceeded maximum connections)", peer_ip);
                        self.send(peer_ip, &Message::Disconnect).await;
                    }
                }

                // Skip if the number of connected peers is above the minimum threshold.
                match self.num_connected_peers() < E::MINIMUM_NUMBER_OF_PEERS {
                    true => trace!("Sending request for more peer connections"),
                    false => return,
                };

                // If the current node is not a sync node, add the sync nodes to the list of candidate peers.
                if E::NODE_TYPE != NodeType::Sync {
                    let sync_nodes: Vec<SocketAddr> = E::SYNC_NODES.iter().map(|ip| ip.parse().unwrap()).collect();
                    self.add_candidate_peers(&sync_nodes);
                }

                // Add the peer nodes to the list of candidate peers.
                let peer_nodes: Vec<SocketAddr> = E::PEER_NODES.iter().map(|ip| ip.parse().unwrap()).collect();
                self.add_candidate_peers(&peer_nodes);

                // Attempt to connect to more peers if the number of connected peers is below the minimum threshold.
                // Select the peers randomly from the list of candidate peers.
                for peer_ip in self
                    .candidate_peers()
                    .iter()
                    .choose_multiple(&mut OsRng::default(), E::MINIMUM_NUMBER_OF_PEERS)
                {
                    if !self.is_connected_to(*peer_ip) {
                        trace!("Attempting connection to {}...", peer_ip);
                        let request = PeersRequest::Connect(*peer_ip, ledger_router.clone());
                        if let Err(error) = peers_router.send(request).await {
                            error!("Failed to transmit the request: '{}'", error);
                        }
                    }
                }
                // Request more peers if the number of connected peers is below the threshold.
                self.propagate(self.local_ip, &Message::PeerRequest).await;
            }
            PeersRequest::MessagePropagate(sender, message) => {
                self.propagate(sender, &message).await;
            }
            PeersRequest::MessageSend(sender, message) => {
                self.send(sender, &message).await;
            }
            PeersRequest::PeerConnecting(stream, peer_ip, ledger_router) => {
                // Ensure the peer IP is not this node.
                if peer_ip == self.local_ip
                    || (peer_ip.ip().is_unspecified() || peer_ip.ip().is_loopback()) && peer_ip.port() == self.local_ip.port()
                {
                    debug!("Skipping connection request to {} (attempted to self-connect)", peer_ip);
                }
                // Ensure the node does not surpass the maximum number of peer connections.
                else if self.num_connected_peers() >= E::MAXIMUM_NUMBER_OF_PEERS {
                    debug!("Dropping connection request from {} (maximum peers reached)", peer_ip);
                }
                // Ensure the node is not already connected to this peer.
                else if self.is_connected_to(peer_ip) {
                    debug!("Dropping connection request from {} (already connected)", peer_ip);
                }
                // Ensure the peer is not restricted.
                else if self.is_restricted(peer_ip) {
                    debug!("Dropping connection request from {} (restricted)", peer_ip);
                }
                // Spawn a handler to be run asynchronously.
                else {
                    // Sanitize the port from the peer, if it is a remote IP address.
                    let (peer_lookup, peer_port) = match peer_ip.ip().is_loopback() {
                        // Loopback case - Do not sanitize, merely pass through.
                        true => (peer_ip, peer_ip.port()),
                        // Remote case - Sanitize, storing u16::MAX for the peer IP address to dedup the peer next time.
                        false => (SocketAddr::new(peer_ip.ip(), u16::MAX), peer_ip.port()),
                    };

                    // Fetch the inbound tracker entry for this peer.
                    let ((initial_port, num_attempts), last_seen) = self
                        .seen_inbound_connections
                        .entry(peer_lookup)
                        .or_insert(((peer_port, 0), SystemTime::UNIX_EPOCH));
                    let elapsed = last_seen.elapsed().unwrap_or(Duration::MAX).as_secs();

                    // Reset the inbound tracker entry for this peer, if the predefined elapsed time has passed.
                    if elapsed > E::RADIO_SILENCE_IN_SECS {
                        // Reset the initial port for this peer.
                        *initial_port = peer_port;
                        // Reset the number of attempts for this peer.
                        *num_attempts = 0;
                        // Reset the last seen timestamp for this peer.
                        *last_seen = SystemTime::now();
                    }

                    // Ensure the connecting peer has not surpassed the connection attempt limit.
                    if *initial_port < peer_port && *num_attempts > E::MAXIMUM_CONNECTION_FAILURES {
                        trace!("Dropping connection request from {} (tried {} secs ago)", peer_ip, elapsed);
                    } else {
                        debug!("Received a connection request from {}", peer_ip);
                        // Update the number of attempts for this peer.
                        *num_attempts += 1;
                        // Initialize the peer handler.
                        Peer::handler(
                            stream,
                            self.local_ip,
                            self.local_nonce,
                            peers_router,
                            ledger_router,
                            &mut self.connected_nonces(),
                        )
                        .await;
                    }
                }
            }
            PeersRequest::PeerConnected(peer_ip, peer_nonce, outbound) => {
                // Add an entry for this `Peer` in the connected peers.
                self.connected_peers.insert(peer_ip, (peer_nonce, outbound));
                // Remove an entry for this `Peer` in the candidate peers, if it exists.
                self.candidate_peers.remove(&peer_ip);
            }
            PeersRequest::PeerDisconnected(peer_ip) => {
                // Remove an entry for this `Peer` in the connected peers, if it exists.
                self.connected_peers.remove(&peer_ip);
                // Add an entry for this `Peer` in the candidate peers.
                self.candidate_peers.insert(peer_ip);

                // Clear the peer's seen blocks/transactions.
                self.seen_outbound_blocks.remove(&peer_ip);
                self.seen_outbound_transactions.remove(&peer_ip);
            }
            PeersRequest::PeerRestricted(peer_ip) => {
                // Add an entry for this `Peer` in the restricted peers.
                self.restricted_peers.insert(peer_ip, Instant::now());
            }
            PeersRequest::SendPeerResponse(recipient) => {
                // Send a `PeerResponse` message.
                self.send(recipient, &Message::PeerResponse(self.connected_peers())).await;
            }
            PeersRequest::ReceivePeerResponse(peer_ips) => {
                self.add_candidate_peers(&peer_ips);
            }
        }
    }

    ///
    /// Adds the given peer IPs to the set of candidate peers.
    ///
    /// This method skips adding any given peers if the combined size exceeds the threshold,
    /// as the peer providing this list could be subverting the protocol.
    ///
    fn add_candidate_peers(&mut self, peers: &[SocketAddr]) {
        // Ensure the combined number of peers does not surpass the threshold.
        if self.candidate_peers.len() + peers.len() < E::MAXIMUM_CANDIDATE_PEERS {
            // Proceed to insert each new candidate peer IP.
            for peer_ip in peers.iter().take(E::MAXIMUM_CANDIDATE_PEERS) {
                // Ensure the peer is not self and is a new candidate peer.
                let is_self = *peer_ip == self.local_ip
                    || (peer_ip.ip().is_unspecified() || peer_ip.ip().is_loopback()) && peer_ip.port() == self.local_ip.port();
                if !is_self && !self.is_connected_to(*peer_ip) && !self.candidate_peers.contains(peer_ip) {
                    self.candidate_peers.insert(*peer_ip);
                }
            }
        }
    }

    ///
    /// Sends the given message to specified peer.
    ///
    async fn send(&mut self, peer: SocketAddr, message: &Message<N, E>) {
        match self.connected_peers.get(&peer) {
            Some((_, outbound)) => {
                // Ensure sufficient time has passed before needing to send the message.
                let is_ready_to_send = match message {
                    Message::UnconfirmedBlock(block) => {
                        // Retrieve the last seen timestamp of this block for this peer.
                        let seen_blocks = self.seen_outbound_blocks.entry(peer).or_insert_with(Default::default);
                        let last_seen = seen_blocks.entry(block.hash()).or_insert(SystemTime::UNIX_EPOCH);
                        let is_ready_to_send = last_seen.elapsed().unwrap().as_secs() > E::RADIO_SILENCE_IN_SECS;

                        // Update the timestamp for the peer and sent block.
                        seen_blocks.insert(block.hash(), SystemTime::now());
                        // Report the unconfirmed block height.
                        if is_ready_to_send {
                            trace!("Preparing to send '{} {}' to {}", message.name(), block.height(), peer);
                        }
                        is_ready_to_send
                    }
                    Message::UnconfirmedTransaction(transaction) => {
                        // Retrieve the last seen timestamp of this transaction for this peer.
                        let seen_transactions = self.seen_outbound_transactions.entry(peer).or_insert_with(Default::default);
                        let last_seen = seen_transactions
                            .entry(transaction.transaction_id())
                            .or_insert(SystemTime::UNIX_EPOCH);
                        let is_ready_to_send = last_seen.elapsed().unwrap().as_secs() > E::RADIO_SILENCE_IN_SECS;

                        // Update the timestamp for the peer and sent transaction.
                        seen_transactions.insert(transaction.transaction_id(), SystemTime::now());
                        // Report the unconfirmed block height.
                        if is_ready_to_send {
                            trace!(
                                "Preparing to send '{} {}' to {}",
                                message.name(),
                                transaction.transaction_id(),
                                peer
                            );
                        }
                        is_ready_to_send
                    }
                    _ => true,
                };
                // Send the message if it is ready.
                if is_ready_to_send {
                    if let Err(error) = outbound.send(message.clone()).await {
                        trace!("Outbound channel failed: {}", error);
                        self.connected_peers.remove(&peer);
                    }
                }
            }
            None => warn!("Attempted to send to a non-connected peer {}", peer),
        }
    }

    ///
    /// Sends the given message to every connected peer, excluding the sender.
    ///
    async fn propagate(&mut self, sender: SocketAddr, message: &Message<N, E>) {
        // Iterate through all peers that are not the sender.
        for peer in self.connected_peers().iter() {
            // Ensure the sender is not this peer.
            if peer != &sender {
                self.send(*peer, message).await;
            }
        }
    }
}

// TODO (howardwu): Consider changing this to a random challenge height.
//  The tradeoff is that checking genesis ensures your peer is starting at the same genesis block.
//  Choosing a random height also requires knowing upfront the height of the peer.
//  As such, leaving it at the genesis block height may be the best option here.
const CHALLENGE_HEIGHT: u32 = 0;

///
/// The state for each connected client.
///
struct Peer<N: Network, E: Environment> {
    /// The IP address of the peer, with the port set to the listener port.
    listener_ip: SocketAddr,
    /// The message version of the peer.
    version: u32,
    /// The timestamp of the last message received from this peer.
    last_seen: Instant,
    /// The TCP socket that handles sending and receiving data with this peer.
    outbound_socket: Framed<TcpStream, Message<N, E>>,
    /// The `outbound_handler` half of the MPSC message channel, used to receive messages from peers.
    /// When a message is received on this `OutboundHandler`, it will be written to the socket.
    outbound_handler: OutboundHandler<N, E>,
    /// The map of block hashes to their last seen timestamp.
    seen_inbound_blocks: HashMap<N::BlockHash, SystemTime>,
    /// The map of transaction IDs to their last seen timestamp.
    seen_inbound_transactions: HashMap<N::TransactionID, SystemTime>,
}

impl<N: Network, E: Environment> Peer<N, E> {
    /// Create a new instance of `Peer`.
    async fn new(
        stream: TcpStream,
        local_ip: SocketAddr,
        local_nonce: u64,
        peers_router: &PeersRouter<N, E>,
        ledger_router: &LedgerRouter<N, E>,
        connected_nonces: &[u64],
    ) -> Result<Self> {
        // Construct the socket.
        let mut outbound_socket = Framed::new(stream, Message::<N, E>::PeerRequest);

        // Perform the handshake before proceeding.
        let (peer_ip, peer_nonce) = Peer::handshake(&mut outbound_socket, local_ip, local_nonce, connected_nonces).await?;

        // Send the first ping sequence to the peer.
        ledger_router.send(LedgerRequest::SendPing(peer_ip)).await?;

        // Create a channel for this peer.
        let (outbound_router, outbound_handler) = mpsc::channel(1024);

        // Add an entry for this `Peer` in the connected peers.
        peers_router
            .send(PeersRequest::PeerConnected(peer_ip, peer_nonce, outbound_router))
            .await?;

        Ok(Peer {
            listener_ip: peer_ip,
            version: 0,
            last_seen: Instant::now(),
            outbound_socket,
            outbound_handler,
            seen_inbound_blocks: Default::default(),
            seen_inbound_transactions: Default::default(),
        })
    }

    /// Returns the IP address of the peer, with the port set to the listener port.
    fn peer_ip(&self) -> SocketAddr {
        self.listener_ip
    }

    /// Sends the given message to this peer.
    async fn send(&mut self, message: Message<N, E>) -> Result<()> {
        trace!("Sending '{}' to {}", message.name(), self.peer_ip());
        self.outbound_socket.send(message).await?;
        Ok(())
    }

    /// Performs the handshake protocol, returning the listener IP and nonce of the peer upon success.
    async fn handshake(
        outbound_socket: &mut Framed<TcpStream, Message<N, E>>,
        local_ip: SocketAddr,
        local_nonce: u64,
        connected_nonces: &[u64],
    ) -> Result<(SocketAddr, u64)> {
        // Get the IP address of the peer.
        let mut peer_ip = outbound_socket.get_ref().peer_addr()?;

        // Retrieve the genesis block header.
        let genesis_block_header = N::genesis_block().header();

        // Send a challenge request to the peer.
        let message = Message::<N, E>::ChallengeRequest(E::MESSAGE_VERSION, local_ip.port(), local_nonce, CHALLENGE_HEIGHT);
        trace!("Sending '{}-A' to {}", message.name(), peer_ip);
        outbound_socket.send(message).await?;

        // Wait for the counterparty challenge request to come in.
        let peer_nonce = match outbound_socket.next().await {
            Some(Ok(message)) => {
                // Process the message.
                trace!("Received '{}-B' from {}", message.name(), peer_ip);
                match message {
                    Message::ChallengeRequest(version, listener_port, peer_nonce, _block_height) => {
                        // Ensure the message protocol version is not outdated.
                        if version < E::MESSAGE_VERSION {
                            warn!("Dropping {} on version {} (outdated)", peer_ip, version);
                            return Err(anyhow!("Dropping {} on version {} (outdated)", peer_ip, version));
                        }
                        // Verify the listener port.
                        if peer_ip.port() != listener_port {
                            // Update the peer IP to the listener port.
                            peer_ip.set_port(listener_port);
                            // Ensure the claimed listener port is open.
                            let stream =
                                match timeout(Duration::from_secs(E::CONNECTION_TIMEOUT_IN_SECS), TcpStream::connect(peer_ip)).await {
                                    Ok(stream) => stream,
                                    Err(error) => return Err(anyhow!("Unable to reach '{}': '{:?}'", peer_ip, error)),
                                };
                            // Error if the stream is not open.
                            if let Err(error) = stream {
                                return Err(anyhow!("Unable to reach '{}': '{}'", peer_ip, error));
                            }
                        }
                        // Ensure the peer is not this node.
                        if local_nonce == peer_nonce {
                            return Err(anyhow!("Attempted to connect to self (nonce = {})", peer_nonce));
                        }
                        // Ensure the peer is not already connected to this node.
                        if connected_nonces.contains(&peer_nonce) {
                            return Err(anyhow!("Already connected to a peer with nonce {}", peer_nonce));
                        }
                        // Send the challenge response.
                        let message = Message::ChallengeResponse(genesis_block_header.clone());
                        trace!("Sending '{}-B' to {}", message.name(), peer_ip);
                        outbound_socket.send(message).await?;

                        peer_nonce
                    }
                    message => {
                        return Err(anyhow!(
                            "Expected challenge request, received '{}' from {}",
                            message.name(),
                            peer_ip
                        ));
                    }
                }
            }
            // An error occurred.
            Some(Err(error)) => return Err(anyhow!("Failed to get challenge request from {}: {:?}", peer_ip, error)),
            // Did not receive anything.
            None => return Err(anyhow!("Dropped prior to challenge request of {}", peer_ip)),
        };

        // Wait for the challenge response to come in.
        match outbound_socket.next().await {
            Some(Ok(message)) => {
                // Process the message.
                trace!("Received '{}-A' from {}", message.name(), peer_ip);
                match message {
                    Message::ChallengeResponse(block_header) => {
                        match block_header.height() == CHALLENGE_HEIGHT && &block_header == genesis_block_header && block_header.is_valid()
                        {
                            true => Ok((peer_ip, peer_nonce)),
                            false => return Err(anyhow!("Challenge response from {} failed, received '{}'", peer_ip, block_header)),
                        }
                    }
                    message => {
                        return Err(anyhow!(
                            "Expected challenge response, received '{}' from {}",
                            message.name(),
                            peer_ip
                        ));
                    }
                }
            }
            // An error occurred.
            Some(Err(error)) => return Err(anyhow!("Failed to get challenge response from {}: {:?}", peer_ip, error)),
            // Did not receive anything.
            None => return Err(anyhow!("Failed to get challenge response from {}, peer has disconnected", peer_ip)),
        }
    }

    /// A handler to process an individual peer.
    async fn handler<'a, T: Iterator<Item = &'a u64> + Send>(
        stream: TcpStream,
        local_ip: SocketAddr,
        local_nonce: u64,
        peers_router: &PeersRouter<N, E>,
        ledger_router: LedgerRouter<N, E>,
        connected_nonces: &mut T,
    ) {
        let connected_nonces = connected_nonces.cloned().collect::<Vec<u64>>();
        let peers_router = peers_router.clone();
        task::spawn(async move {
            // Register our peer with state which internally sets up some channels.
            let mut peer = match Peer::new(stream, local_ip, local_nonce, &peers_router, &ledger_router, &connected_nonces).await {
                Ok(peer) => peer,
                Err(error) => {
                    trace!("{}", error);
                    return;
                }
            };

            // Retrieve the peer IP.
            let peer_ip = peer.peer_ip();
            info!("Connected to {}", peer_ip);

            // Process incoming messages until this stream is disconnected.
            loop {
                tokio::select! {
                    // Message channel is routing a message outbound to the peer.
                    Some(message) = peer.outbound_handler.recv() => {
                        // Disconnect if the peer has not communicated back within the predefined time.
                        if peer.last_seen.elapsed() > Duration::from_secs(E::RADIO_SILENCE_IN_SECS) {
                            warn!("Peer {} has not communicated in {} seconds", peer_ip, peer.last_seen.elapsed().as_secs());
                            break;
                        } else {
                            // Route a message to the peer.
                            if let Err(error) = peer.send(message).await {
                                warn!("[OutboundRouter] {}", error);
                            }
                        }
                    }
                    result = peer.outbound_socket.next() => match result {
                        // Received a message from the peer.
                        Some(Ok(message)) => {
                            // Disconnect if the peer has not communicated back within the predefined time.
                            match peer.last_seen.elapsed() > Duration::from_secs(E::RADIO_SILENCE_IN_SECS) {
                                true => {
                                    let last_seen = peer.last_seen.elapsed().as_secs();
                                    warn!("Failed to receive a message from {} in {} seconds", peer_ip, last_seen);
                                    break;
                                },
                                false => {
                                    // Update the last seen timestamp.
                                    peer.last_seen = Instant::now();
                                }
                            }
                            // Process the message.
                            trace!("Received '{}' from {}", message.name(), peer_ip);
                            match message {
                                Message::BlockRequest(start_block_height, end_block_height) => {
                                    // Route the `BlockRequest` to the ledger.
                                    if let Err(error) = ledger_router.send(LedgerRequest::BlockRequest(peer_ip, start_block_height, end_block_height)).await {
                                        warn!("[BlockRequest] {}", error);
                                    }
                                },
                                Message::BlockResponse(block) => {
                                    // Route the `BlockResponse` to the ledger.
                                    if let Err(error) = ledger_router.send(LedgerRequest::BlockResponse(peer_ip, block)).await {
                                        warn!("[BlockResponse] {}", error);
                                    }
                                }
                                Message::ChallengeRequest(..) | Message::ChallengeResponse(..) => {
                                    // Peer is not following the protocol.
                                    warn!("Peer {} is not following the protocol", peer_ip);
                                    break;
                                },
                                Message::Disconnect => {
                                    // Route the `Disconnect` to the ledger.
                                    if let Err(error) = ledger_router.send(LedgerRequest::Disconnect(peer_ip)).await {
                                        warn!("[Disconnect] {}", error);
                                    }
                                    break;
                                }
                                Message::PeerRequest => {
                                    // Send a `PeerResponse` message.
                                    if let Err(error) = peers_router.send(PeersRequest::SendPeerResponse(peer_ip)).await {
                                        warn!("[PeerRequest] {}", error);
                                    }
                                }
                                Message::PeerResponse(peer_ips) => {
                                    // Adds the given peer IPs to the list of candidate peers.
                                    if let Err(error) = peers_router.send(PeersRequest::ReceivePeerResponse(peer_ips)).await {
                                        warn!("[PeerResponse] {}", error);
                                    }
                                }
                                Message::Ping(version, block_height, block_hash) => {
                                    // Ensure the message protocol version is not outdated.
                                    if version < E::MESSAGE_VERSION {
                                        warn!("Dropping {} on version {} (outdated)", peer_ip, version);
                                        break;
                                    }
                                    // Update the version of the peer.
                                    peer.version = version;
                                    // Route the `Ping` to the ledger.
                                    if let Err(error) = ledger_router.send(LedgerRequest::Ping(peer_ip, block_height, block_hash)).await {
                                        warn!("[Ping] {}", error);
                                    }
                                },
                                Message::Pong(is_fork, block_locators) => {
                                    // Route the `Pong` to the ledger.
                                    if let Err(error) = ledger_router.send(LedgerRequest::Pong(peer_ip, is_fork, block_locators)).await {
                                        warn!("[Pong] {}", error);
                                    }
                                }
                                Message::UnconfirmedBlock(block) => {
                                    // Drop the peer, if they have sent more than 5 unconfirmed blocks in the last 5 seconds.
                                    let frequency = peer.seen_inbound_blocks.values().filter(|t| t.elapsed().unwrap().as_secs() <= 5).count();
                                    if frequency >= 5 {
                                        warn!("Dropping {} for spamming unconfirmed blocks (frequency = {})", peer_ip, frequency);
                                        // Send a `PeerRestricted` message.
                                        if let Err(error) = peers_router.send(PeersRequest::PeerRestricted(peer_ip)).await {
                                            warn!("[PeerRestricted] {}", error);
                                        }
                                        break;
                                    }

                                    // Retrieve the last seen timestamp of the received block.
                                    let last_seen = peer.seen_inbound_blocks.entry(block.hash()).or_insert(SystemTime::UNIX_EPOCH);
                                    let is_ready_to_route = last_seen.elapsed().unwrap().as_secs() > E::RADIO_SILENCE_IN_SECS;

                                    // Update the timestamp for the received block.
                                    peer.seen_inbound_blocks.insert(block.hash(), SystemTime::now());
                                    // Route the `UnconfirmedBlock` to the ledger.
                                    match is_ready_to_route {
                                        true => if let Err(error) = ledger_router.send(LedgerRequest::UnconfirmedBlock(peer_ip, block)).await {
                                            warn!("[UnconfirmedBlock] {}", error);
                                        },
                                        false => trace!("Skipping 'UnconfirmedBlock {}' from {}", block.height(), peer_ip)
                                    }
                                }
                                Message::UnconfirmedTransaction(transaction) => {
                                    // Drop the peer, if they have sent more than 500 unconfirmed transactions in the last 5 seconds.
                                    let frequency = peer.seen_inbound_transactions.values().filter(|t| t.elapsed().unwrap().as_secs() <= 5).count();
                                    if frequency >= 500 {
                                        warn!("Dropping {} for spamming unconfirmed transactions (frequency = {})", peer_ip, frequency);
                                        // Send a `PeerRestricted` message.
                                        if let Err(error) = peers_router.send(PeersRequest::PeerRestricted(peer_ip)).await {
                                            warn!("[PeerRestricted] {}", error);
                                        }
                                        break;
                                    }

                                    // Retrieve the last seen timestamp of the received transaction.
                                    let last_seen = peer.seen_inbound_transactions.entry(transaction.transaction_id()).or_insert(SystemTime::UNIX_EPOCH);
                                    let is_ready_to_route = last_seen.elapsed().unwrap().as_secs() > E::RADIO_SILENCE_IN_SECS;

                                    // Update the timestamp for the received transaction.
                                    peer.seen_inbound_transactions.insert(transaction.transaction_id(), SystemTime::now());
                                    // Route the `UnconfirmedTransaction` to the ledger.
                                    match is_ready_to_route {
                                        true => if let Err(error) = ledger_router.send(LedgerRequest::UnconfirmedTransaction(peer_ip, transaction)).await {
                                            warn!("[UnconfirmedTransaction] {}", error);
                                        },
                                        false => trace!("Skipping 'UnconfirmedTransaction {}' from {}", transaction.transaction_id(), peer_ip)
                                    }
                                }
                                Message::Unused(_) => break, // Peer is not following the protocol.
                            }
                        }
                        // An error occurred.
                        Some(Err(error)) => error!("Failed to read message from {}: {}", peer_ip, error),
                        // The stream has been disconnected.
                        None => break,
                    },
                }
            }

            // When this is reached, it means the peer has disconnected.
            // Route a `Disconnect` to the ledger.
            if let Err(error) = ledger_router.send(LedgerRequest::Disconnect(peer_ip)).await {
                warn!("[Peer::Disconnect] {}", error);
            }
        });
    }
}
