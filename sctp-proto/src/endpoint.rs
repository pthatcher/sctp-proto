use std::{
    collections::{HashMap, VecDeque},
    iter,
    //fmt,
    net::{IpAddr, SocketAddr},
    ops::{Index, IndexMut},
    sync::Arc,
    time::Instant, //, SystemTime},
};

use crate::association::Association;
use crate::config::{ClientConfig, EndpointConfig, ServerConfig, TransportConfig};
use crate::shared::{
    AssociationEvent, AssociationEventInner, AssociationId, EndpointEvent, EndpointEventInner,
    IssuedAid,
};
use crate::{EcnCodepoint, Payload, Transmit};

use crate::chunk::chunk_type::CT_INIT;
use crate::packet::PartialDecode;
use crate::util::{AssociationIdGenerator, RandomAssociationIdGenerator};
use bytes::Bytes; //, BytesMut, BufMut, Bytes, };
use fxhash::FxHashMap;
use rand::{rngs::StdRng, /*Rng, RngCore,*/ SeedableRng};
use slab::Slab;
use thiserror::Error;
use tracing::{debug, trace}; //{ trace, warn};

/// The main entry point to the library
///
/// This object performs no I/O whatsoever. Instead, it generates a stream of packets to send via
/// `poll_transmit`, and consumes incoming packets and connection-generated events via `handle` and
/// `handle_event`.
pub struct Endpoint {
    rng: StdRng,
    transmits: VecDeque<Transmit>,
    /// Identifies connections based on the INIT Dst AID the peer utilized
    ///
    /// Uses a standard `HashMap` to protect against hash collision attacks.
    connection_ids_init: HashMap<AssociationId, AssociationHandle>,
    /// Identifies connections based on locally created CIDs
    ///
    /// Uses a cheaper hash function since keys are locally created
    connection_ids: FxHashMap<AssociationId, AssociationHandle>,

    connections: Slab<AssociationMeta>,
    local_cid_generator: Box<dyn AssociationIdGenerator>,
    config: Arc<EndpointConfig>,
    server_config: Option<Arc<ServerConfig>>,
    /// Whether incoming connections should be unconditionally rejected by a server
    ///
    /// Equivalent to a `ServerConfig.accept_buffer` of `0`, but can be changed after the endpoint is constructed.
    reject_new_connections: bool,
}

impl Endpoint {
    /// Create a new endpoint
    ///
    /// Returns `Err` if the configuration is invalid.
    pub fn new(config: Arc<EndpointConfig>, server_config: Option<Arc<ServerConfig>>) -> Self {
        Self {
            rng: StdRng::from_entropy(),
            transmits: VecDeque::new(),
            connection_ids_init: HashMap::default(),
            connection_ids: FxHashMap::default(),
            connections: Slab::new(),
            local_cid_generator: (config.aid_generator_factory.as_ref())(),
            reject_new_connections: false,
            config,
            server_config,
        }
    }

    /// Get the next packet to transmit
    #[must_use]
    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.transmits.pop_front()
    }

    /// Replace the server configuration, affecting new incoming associations only
    pub fn set_server_config(&mut self, server_config: Option<Arc<ServerConfig>>) {
        self.server_config = server_config;
    }

    /// Process `EndpointEvent`s emitted from related `Connection`s
    ///
    /// In turn, processing this event may return a `ConnectionEvent` for the same `Connection`.
    pub fn handle_event(
        &mut self,
        ch: AssociationHandle,
        event: EndpointEvent,
    ) -> Option<AssociationEvent> {
        use EndpointEventInner::*;
        match event.0 {
            NeedIdentifiers(now, n) => {
                return Some(self.send_new_identifiers(now, ch, n));
            }
            RetireAssociationId(now, seq, allow_more_aids) => {
                if let Some(cid) = self.connections[ch].loc_cids.remove(&seq) {
                    trace!("peer retired AID {}: {}", seq, cid);
                    self.connection_ids.remove(&cid);
                    if allow_more_aids {
                        return Some(self.send_new_identifiers(now, ch, 1));
                    }
                }
            }
            Drained => {
                let conn = self.connections.remove(ch.0);
                self.connection_ids_init.remove(&conn.init_cid);
                for cid in conn.loc_cids.values() {
                    self.connection_ids.remove(cid);
                }
            }
        }
        None
    }

    /// Process an incoming UDP datagram
    pub fn handle(
        &mut self,
        now: Instant,
        remote: SocketAddr,
        local_ip: Option<IpAddr>,
        ecn: Option<EcnCodepoint>,
        data: Bytes,
    ) -> Option<(AssociationHandle, DatagramEvent)> {
        let partial_decode = match PartialDecode::unmarshal(&data) {
            Ok(x) => x,
            Err(err) => {
                trace!("malformed header: {}", err);
                return None;
            }
        };

        //
        // Handle packet on existing connection, if any
        //
        let dst_cid = partial_decode.common_header.verification_tag;
        let known_ch = if dst_cid > 0 {
            self.connection_ids.get(&dst_cid).cloned()
        } else {
            //TODO: improve INIT handling for DoS attack
            if partial_decode.first_chunk_type == CT_INIT {
                if let Some(dst_cid) = partial_decode.initiate_tag {
                    self.connection_ids.get(&dst_cid).cloned()
                } else {
                    None
                }
            } else {
                None
            }
        };

        if let Some(ch) = known_ch {
            return Some((
                ch,
                DatagramEvent::AssociationEvent(AssociationEvent(AssociationEventInner::Datagram(
                    Transmit {
                        now,
                        remote,
                        ecn,
                        payload: Payload::PartialDecode(partial_decode),
                        local_ip,
                    },
                ))),
            ));
        }

        //
        // Potentially create a new connection
        //
        self.handle_first_packet(now, remote, local_ip, ecn, partial_decode)
            .map(|(ch, a)| (ch, DatagramEvent::NewAssociation(a)))
    }

    /// Initiate a Association
    pub fn connect(
        &mut self,
        config: ClientConfig,
        remote: SocketAddr,
    ) -> Result<(AssociationHandle, Association), ConnectError> {
        if self.is_full() {
            return Err(ConnectError::TooManyAssociations);
        }
        if remote.port() == 0 {
            return Err(ConnectError::InvalidRemoteAddress(remote));
        }

        let remote_aid = RandomAssociationIdGenerator::new().generate_aid();
        let local_aid = self.new_aid();

        let (ch, conn) = self.add_connection(
            remote_aid,
            local_aid,
            remote,
            None,
            Instant::now(),
            None,
            config.transport,
        );
        Ok((ch, conn))
    }

    fn send_new_identifiers(
        &mut self,
        now: Instant,
        ch: AssociationHandle,
        num: u64,
    ) -> AssociationEvent {
        let mut ids = vec![];
        for _ in 0..num {
            let id = self.new_aid();
            self.connection_ids.insert(id, ch);
            let meta = &mut self.connections[ch];
            meta.cids_issued += 1;
            let sequence = meta.cids_issued;
            meta.loc_cids.insert(sequence, id);
            ids.push(IssuedAid { sequence, id });
        }
        AssociationEvent(AssociationEventInner::NewIdentifiers(ids, now))
    }

    fn new_aid(&mut self) -> AssociationId {
        loop {
            let aid = self.local_cid_generator.generate_aid();
            if !self.connection_ids.contains_key(&aid) {
                break aid;
            }
        }
    }

    fn handle_first_packet(
        &mut self,
        now: Instant,
        remote: SocketAddr,
        local_ip: Option<IpAddr>,
        ecn: Option<EcnCodepoint>,
        partial_decode: PartialDecode,
    ) -> Option<(AssociationHandle, Association)> {
        if partial_decode.first_chunk_type != CT_INIT
            || (partial_decode.first_chunk_type == CT_INIT && partial_decode.initiate_tag.is_none())
        {
            debug!("refusing first packet with Non-INIT or emtpy initial_tag INIT");
            return None;
        }

        let server_config = self.server_config.as_ref().unwrap();

        if self.connections.len() >= server_config.concurrent_connections as usize
            || self.reject_new_connections
            || self.is_full()
        {
            debug!("refusing connection");
            //TODO: self.initial_close();
            return None;
        }

        let server_config = server_config.clone();
        let transport_config = server_config.transport.clone();

        let remote_aid = *partial_decode.initiate_tag.as_ref().unwrap();
        let local_aid = self.new_aid();

        let (ch, mut conn) = self.add_connection(
            remote_aid,
            local_aid,
            remote,
            local_ip,
            now,
            Some(server_config),
            transport_config,
        );

        conn.handle_transmit(Transmit {
            now,
            remote,
            ecn,
            payload: Payload::PartialDecode(partial_decode),
            local_ip,
        });

        Some((ch, conn))
    }

    fn add_connection(
        &mut self,
        remote_aid: AssociationId,
        local_aid: AssociationId,
        remote_addr: SocketAddr,
        local_ip: Option<IpAddr>,
        now: Instant,
        server_config: Option<Arc<ServerConfig>>,
        transport_config: Arc<TransportConfig>,
    ) -> (AssociationHandle, Association) {
        let conn = Association::new(
            server_config,
            transport_config,
            local_aid,
            remote_addr,
            local_ip,
            now,
        );

        let id = self.connections.insert(AssociationMeta {
            init_cid: remote_aid,
            cids_issued: 0,
            loc_cids: iter::once((0, local_aid)).collect(),
            initial_remote: remote_addr,
        });

        let ch = AssociationHandle(id);
        self.connection_ids.insert(local_aid, ch);

        (ch, conn)
    }

    /// Unconditionally reject future incoming connections
    pub fn reject_new_connections(&mut self) {
        self.reject_new_connections = true;
    }

    /// Access the configuration used by this endpoint
    pub fn config(&self) -> &EndpointConfig {
        &self.config
    }

    /// Whether we've used up 3/4 of the available AID space
    fn is_full(&self) -> bool {
        (((u32::MAX >> 1) + (u32::MAX >> 2)) as usize) < self.connection_ids.len()
    }
}

#[derive(Debug)]
pub(crate) struct AssociationMeta {
    init_cid: AssociationId,
    /// Number of local connection IDs that have been issued in NEW_CONNECTION_ID frames.
    cids_issued: u64,
    loc_cids: FxHashMap<u64, AssociationId>,
    /// Remote address the connection began with
    ///
    /// Only needed to support connections with zero-length CIDs, which cannot migrate, so we don't
    /// bother keeping it up to date.
    initial_remote: SocketAddr,
}

/// Internal identifier for a `Connection` currently associated with an endpoint
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct AssociationHandle(pub usize);

impl From<AssociationHandle> for usize {
    fn from(x: AssociationHandle) -> usize {
        x.0
    }
}

impl Index<AssociationHandle> for Slab<AssociationMeta> {
    type Output = AssociationMeta;
    fn index(&self, ch: AssociationHandle) -> &AssociationMeta {
        &self[ch.0]
    }
}

impl IndexMut<AssociationHandle> for Slab<AssociationMeta> {
    fn index_mut(&mut self, ch: AssociationHandle) -> &mut AssociationMeta {
        &mut self[ch.0]
    }
}

/// Event resulting from processing a single datagram
#[allow(clippy::large_enum_variant)] // Not passed around extensively
pub enum DatagramEvent {
    /// The datagram is redirected to its `Association`
    AssociationEvent(AssociationEvent),
    /// The datagram has resulted in starting a new `Association`
    NewAssociation(Association),
}

/// Errors in the parameters being used to create a new association
///
/// These arise before any I/O has been performed.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// The endpoint can no longer create new associations
    ///
    /// Indicates that a necessary component of the endpoint has been dropped or otherwise disabled.
    #[error("endpoint stopping")]
    EndpointStopping,
    /// The number of active associations on the local endpoint is at the limit
    ///
    /// Try using longer association IDs.
    #[error("too many associations")]
    TooManyAssociations,
    /// The domain name supplied was malformed
    #[error("invalid DNS name: {0}")]
    InvalidDnsName(String),
    /// The remote [`SocketAddr`] supplied was malformed
    ///
    /// Examples include attempting to connect to port 0, or using an inappropriate address family.
    #[error("invalid remote address: {0}")]
    InvalidRemoteAddress(SocketAddr),
    /// No default client configuration was set up
    ///
    /// Use `Endpoint::connect_with` to specify a client configuration.
    #[error("no default client config")]
    NoDefaultClientConfig,
}
