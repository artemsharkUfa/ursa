//! Ursa Transport implementation.
//!
//!
//!

use async_std::task::block_on;
use libp2p::{
    core::{
        muxing::StreamMuxerBox,
        transport::{upgrade, Boxed, OrTransport},
        upgrade::SelectUpgrade,
    },
    dns::DnsConfig,
    identity::Keypair,
    mplex, noise,
    relay::v2::client::transport::ClientTransport,
    tcp::{GenTcpConfig, TcpTransport},
    yamux, PeerId, Transport,
};

use crate::config::NetworkConfig;

pub struct UrsaTransport;

impl UrsaTransport {
    /// Creates a new [`UrsaTransport`].
    ///
    /// Defaults to QUIC transport over TCP.
    /// If QUIC fails to establish a connection, we fail over to TCP.
    pub fn new(
        keypair: &Keypair,
        config: &NetworkConfig,
        relay_transport: Option<ClientTransport>,
    ) -> Boxed<(PeerId, StreamMuxerBox)> {
        let id_keys = keypair;
        let local_peer_id = PeerId::from(keypair.public());

        let tcp = {
            let noise = {
                let dh_keys = noise::Keypair::<noise::X25519Spec>::new()
                    .into_authentic(id_keys)
                    .expect("Signing libp2p-noise static DH keypair failed.");

                noise::NoiseConfig::xx(dh_keys).into_authenticated()
            };

            let mplex = {
                let mut mplex_config = mplex::MplexConfig::new();
                mplex_config.set_max_buffer_behaviour(mplex::MaxBufferBehaviour::Block);
                mplex_config.set_max_buffer_size(usize::MAX);

                let mut yamux_config = yamux::YamuxConfig::default();
                yamux_config.set_window_update_mode(yamux::WindowUpdateMode::on_read());

                SelectUpgrade::new(yamux_config, mplex_config)
            };

            let tcp = TcpTransport::new(GenTcpConfig::new());
            let tcp = block_on(DnsConfig::system(tcp)).unwrap();

            if let Some(relay) = relay_transport {
                tcp.or_transport(relay)
                    .upgrade(upgrade::Version::V1)
                    .authenticate(noise)
                    .multiplex(mplex)
                    .boxed()
            } else {
                tcp.upgrade(upgrade::Version::V1)
                    .authenticate(noise)
                    .multiplex(mplex)
                    .boxed()
            }
        };

        // let quic = {
        //     // block_on(QuicTransport::new(
        //     //     QuicConfig::new(keypair),
        //     //     quic_addr.unwrap_or("/ip4/0.0.0.0/udp/0/quic".parse().unwrap()),
        //     // ))
        //     // .unwrap()
        //     todo!()
        // }
        // self.quic.or_transport(self.tcp)

        // OrTransport::new(tcp, tcp)
        //     .map(|either_output, _| match either_output {
        //         EitherOutput::First((peer_id, muxer)) => (peer_id, StreamMuxerBox::new(muxer)),
        //         EitherOutput::Second((peer_id, muxer)) => (peer_id, StreamMuxerBox::new(muxer)),
        //     })
        //     .boxed()
        tcp
    }
}
