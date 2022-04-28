//! Fnet Service implementation.
//!
//!
//!

use async_std::task;
use libp2p::{
    identity::Keypair,
    swarm::{ConnectionLimits, SwarmBuilder},
    PeerId, Swarm,
};
use tracing::trace;

use crate::{behaviour::FnetBehaviour, config::FnetConfig, transport::FnetTransport};

const PROTOCOL_NAME: &[u8] = b"/fnet/0.0.1";

pub struct FnetService {
    swarm: Swarm<FnetBehaviour>,
}

impl FnetService {
    /// Init a new [`FnetService`] based on [`FnetConfig`]
    ///
    /// For fnet [identity] we use ed25519 either
    /// checking for a local store or creating a new keypair.
    ///
    /// For fnet [transport] we build a default QUIC layer and
    /// failover to tcp.
    ///
    /// For fnet behaviour we use [`FnetBehaviour`].
    ///
    /// We construct a [`Swarm`] with [`FnetTransport`] and [`FnetBehaviour`]
    /// listening on [`FnetConfig`] `swarm_addr`.
    ///
    ///
    pub fn new(config: FnetConfig) -> Self {
        // Todo: Create or get from local store
        let keypair = Keypair::generate_ed25519();
        let local_peer_id = PeerId::from(keypair.public());

        let transport = FnetTransport::new(&keypair).build();

        let behaviour = FnetBehaviour::new(&keypair);

        let limits = ConnectionLimits::default()
            .with_max_pending_incoming(todo!())
            .with_max_pending_outgoing(todo!())
            .with_max_established_incoming(todo!())
            .with_max_established_outgoing(todo!())
            .with_max_established(todo!())
            .with_max_established_per_peer(todo!());

        let mut swarm = SwarmBuilder::new(transport, behaviour, local_peer_id)
            // .notify_handler_buffer_size(todo!())
            // .connection_event_buffer_size(todo!())
            .connection_limits(limits)
            .executor(Box::new(|f| {
                task::spawn(f);
            }))
            .build();

        match Swarm::listen_on(&mut swarm, config.swarm_addr) {
            Ok(listener_id) => todo!(),
            Err(error) => todo!(),
        };

        // subscribe to topics and
        // bootstrap node using Kademlia

        FnetService { swarm }
    }
}

#[cfg(test)]
mod tests {}
