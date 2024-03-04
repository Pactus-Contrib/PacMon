use ansi_term::Style;
use futures::executor::block_on;
use futures::future::{Either, FutureExt, TryFutureExt};
use futures::stream::StreamExt;
use libp2p::core::muxing::StreamMuxerBox;
use libp2p::core::transport::OrTransport;
use libp2p::core::transport::Transport;
use libp2p::core::upgrade;
use libp2p::core::ConnectedPoint;
use libp2p::identify;
use libp2p::identity::Keypair;
use libp2p::kad::ProgressStep;
use libp2p::kad::{
    record::store::MemoryStore, GetClosestPeersOk, Kademlia, KademliaConfig, KademliaEvent,
    QueryResult,
};
use libp2p::ping;
use libp2p::relay;
use libp2p::swarm::{self, SwarmBuilder, SwarmEvent};
use libp2p::{core, StreamProtocol};
use libp2p::{dns, noise, swarm::NetworkBehaviour, tcp, yamux, Multiaddr, PeerId, Swarm};
use libp2p_mplex as mplex;
use log::debug;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;
use structopt::clap::arg_enum;
use structopt::StructOpt;
use thiserror::Error;

#[derive(Debug, StructOpt)]
#[structopt(name = "libp2p-lookup", about = "Lookup libp2p nodes.")]

pub enum Connection {
    Ok,
    Faild,
}
pub async fn get_data(address: Multiaddr) -> Connection {
    let file_path = "network_key";

    let local_key = if Path::new(&file_path).exists() {
        // File exists, read the key from the file
        let mut file = File::open(file_path).expect("Error opening file");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();

        Keypair::ed25519_from_bytes(bytes).unwrap()
    } else {
        let key = Keypair::generate_ed25519();

        let mut file = File::create(file_path).expect("Error creating file");
        file.write_all(key.clone().try_into_ed25519().unwrap().secret().as_ref())
            .expect("Error writing key to file");

        key
    };

    // Create a random key for ourselves.
    let local_peer_id = PeerId::from(local_key.public());

    println!("Local peer id: {local_peer_id}");

    for n in 0..5 {
        println!("try to get info {}", n + 1);

        let client = LookupClient::new(local_key.clone(), None);
        let lookup = client.lookup_directly(address.clone()).boxed();
        let timed_lookup = async_std::future::timeout(std::time::Duration::from_secs(5), lookup)
            .map_err(|_| LookupError::Timeout);

        match timed_lookup.await {
            Ok(Ok(peer)) => {
                println!("Lookup for peer with id {:?} succeeded.", peer.peer_id);
                println!("\n{peer}");
                return Connection::Ok;
            }
            Ok(Err(e)) | Err(e) => {
                eprintln!("Lookup failed: {e}");
                std::thread::sleep(Duration::from_millis(3000));
            }
        }
    }
    return Connection::Faild;
}

fn print_key(k: &str, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    writeln!(f, "{}:", Style::new().bold().paint(k))
}

fn print_key_value<V: std::fmt::Debug>(
    k: &str,
    v: V,
    f: &mut std::fmt::Formatter<'_>,
) -> std::fmt::Result {
    writeln!(f, "{}: {v:?}", Style::new().bold().paint(k))
}

pub struct LookupClient {
    swarm: Swarm<LookupBehaviour>,
}

struct Peer {
    peer_id: PeerId,
    protocol_version: String,
    agent_version: String,
    listen_addrs: Vec<Multiaddr>,
    protocols: Vec<StreamProtocol>,
    observed_addr: Multiaddr,
}

impl std::fmt::Display for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        print_key_value("Protocol version", self.protocol_version.clone(), f)?;
        print_key_value("Agent version", self.agent_version.clone(), f)?;
        print_key_value("Observed address", self.observed_addr.clone(), f)?;
        if !self.listen_addrs.is_empty() {
            print_key("Listen addresses", f)?;
            for addr in &self.listen_addrs {
                writeln!(f, "\t- {addr:?}")?;
            }
        }
        if !self.protocols.is_empty() {
            print_key("Protocols", f)?;
            for protocol in &self.protocols {
                writeln!(f, "\t- {protocol}")?;
            }
        }

        Ok(())
    }
}

impl LookupClient {
    fn new(local_key: Keypair, network: Option<Network>) -> Self {
        let local_peer_id = PeerId::from(local_key.public());
        let (relay_transport, relay_client) = relay::client::new(local_peer_id);

        let transport = {
            let authentication_config = noise::Config::new(&local_key).unwrap();

            let multiplexing_config = {
                let mut mplex_config = mplex::MplexConfig::new();
                mplex_config.set_max_buffer_behaviour(mplex::MaxBufferBehaviour::Block);
                mplex_config.set_max_buffer_size(usize::MAX);

                let mut yamux_config = yamux::Config::default();
                // Enable proper flow-control: window updates are only sent when
                // buffered data has been consumed.
                yamux_config.set_window_update_mode(yamux::WindowUpdateMode::on_read());

                core::upgrade::SelectUpgrade::new(yamux_config, mplex_config)
            };

            let tcp_and_relay_transport = OrTransport::new(
                relay_transport,
                tcp::async_io::Transport::new(tcp::Config::new().port_reuse(true).nodelay(true)),
            )
            .upgrade(upgrade::Version::V1)
            .authenticate(authentication_config.clone())
            .multiplex(multiplexing_config.clone())
            .timeout(Duration::from_secs(20));

            let quic_transport = {
                let mut config = libp2p_quic::Config::new(&local_key);
                config.support_draft_29 = true;
                libp2p_quic::async_std::Transport::new(config)
            };

            let tcp_and_relay_and_quic = block_on(dns::DnsConfig::system(
                libp2p::core::transport::OrTransport::new(quic_transport, tcp_and_relay_transport),
            ))
            .unwrap()
            .map(|either_output, _| match either_output {
                Either::Left((peer_id, muxer)) => (peer_id, StreamMuxerBox::new(muxer)),
                Either::Right((peer_id, muxer)) => (peer_id, StreamMuxerBox::new(muxer)),
            })
            .map_err(|err| io::Error::new(io::ErrorKind::Other, err));

            let websocket = libp2p::websocket::WsConfig::new(
                block_on(libp2p::dns::DnsConfig::system(
                    libp2p::tcp::async_io::Transport::new(libp2p::tcp::Config::default()),
                ))
                .unwrap(),
            )
            .upgrade(upgrade::Version::V1)
            .authenticate(authentication_config)
            .multiplex(multiplexing_config)
            .timeout(Duration::from_secs(20));

            OrTransport::new(websocket, tcp_and_relay_and_quic)
                .map(|either_output, _| match either_output {
                    Either::Left((peer_id, muxer)) => (peer_id, StreamMuxerBox::new(muxer)),
                    Either::Right((peer_id, muxer)) => (peer_id, StreamMuxerBox::new(muxer)),
                })
                .boxed()
        };

        let behaviour = {
            let local_peer_id = PeerId::from(local_key.public());

            // Create a Kademlia behaviour.
            let store = MemoryStore::new(local_peer_id);
            let mut kademlia_config = KademliaConfig::default();
            if let Some(protocol_name) = network.clone().and_then(|n| n.protocol()) {
                kademlia_config
                    .set_protocol_names(vec![
                        StreamProtocol::try_from_owned(protocol_name).unwrap()
                    ]);
            }
            let kademlia = Kademlia::with_config(local_peer_id, store, kademlia_config);

            let ping = ping::Behaviour::new(ping::Config::new());

            let user_agent =
                "substrate-node/v2.0.0-e3245d49d-x86_64-linux-gnu (unknown)".to_string();
            let proto_version = "/substrate/1.0".to_string();
            let identify = identify::Behaviour::new(
                identify::Config::new(proto_version, local_key.public())
                    .with_agent_version(user_agent),
            );

            LookupBehaviour {
                kademlia,
                ping,
                identify,
                relay: relay_client,
                keep_alive: swarm::keep_alive::Behaviour,
            }
        };
        let mut swarm = SwarmBuilder::with_executor(
            transport,
            behaviour,
            local_peer_id,
            Box::new(|fut| {
                async_std::task::spawn(fut);
            }),
        )
        .build();

        if let Some(network) = network {
            for (addr, peer_id) in network.bootnodes() {
                swarm.behaviour_mut().kademlia.add_address(&peer_id, addr);
            }
        }

        LookupClient { swarm }
    }

    async fn lookup_directly(mut self, dst_addr: Multiaddr) -> Result<Peer, LookupError> {
        self.swarm.dial(dst_addr.clone()).unwrap();

        loop {
            match self.swarm.next().await.expect("Infinite Stream.") {
                SwarmEvent::ConnectionEstablished {
                    peer_id,
                    endpoint,
                    num_established,
                    concurrent_dial_errors: _,
                    established_in: _,
                    connection_id: _,
                } => {
                    assert_eq!(Into::<u32>::into(num_established), 1);
                    match endpoint {
                        ConnectedPoint::Dialer {
                            address,
                            role_override: _,
                        } => {
                            if address == dst_addr {
                                return self.wait_for_identify(peer_id).await;
                            }
                        }
                        ConnectedPoint::Listener { .. } => {}
                    }
                }
                SwarmEvent::OutgoingConnectionError {
                    peer_id: _,
                    connection_id: _,
                    error,
                } => return Err(LookupError::FailedToDialPeer { error }),
                SwarmEvent::Dialing { .. } => {}
                SwarmEvent::Behaviour(_) => {
                    // Ignore any behaviour events until we are connected to the
                    // destination peer. These should be events from the
                    // connection to a relay only.
                }
                e => panic!("panic: {:?}", e),
            }
        }
    }

    async fn lookup_on_dht(mut self, peer: PeerId) -> Result<Peer, LookupError> {
        self.swarm.behaviour_mut().kademlia.get_closest_peers(peer);

        loop {
            match self.swarm.next().await.expect("Infinite Stream.") {
                SwarmEvent::ConnectionEstablished {
                    peer_id,
                    num_established,
                    ..
                } => {
                    assert_eq!(Into::<u32>::into(num_established), 1);
                    if peer_id == peer {
                        println!("connection established with {}", peer_id);
                        return self.wait_for_identify(peer).await;
                    }
                }
                SwarmEvent::Behaviour(LookupBehaviourEvent::Kademlia(
                    KademliaEvent::OutboundQueryProgressed {
                        result: QueryResult::Bootstrap(_),
                        ..
                    },
                )) => {
                    panic!("Unexpected bootstrap.");
                }
                SwarmEvent::Behaviour(LookupBehaviourEvent::Kademlia(
                    KademliaEvent::OutboundQueryProgressed {
                        result: QueryResult::GetClosestPeers(Ok(GetClosestPeersOk { peers, .. })),
                        step: ProgressStep { count: _, last },
                        ..
                    },
                )) => {
                    if peers.contains(&peer) {
                        if !Swarm::is_connected(&self.swarm, &peer) {
                            // TODO: Kademlia might not be caching the address of the peer.
                            Swarm::dial(&mut self.swarm, peer).unwrap();
                        }

                        return self.wait_for_identify(peer).await;
                    }

                    if last {
                        return Err(LookupError::FailedToFindPeerOnDht);
                    }
                }
                _ => {}
            }
        }
    }

    async fn wait_for_identify(&mut self, peer: PeerId) -> Result<Peer, LookupError> {
        loop {
            match self.swarm.next().await.expect("Infinite Stream.") {
                SwarmEvent::Behaviour(LookupBehaviourEvent::Identify(
                    identify::Event::Received {
                        peer_id,
                        info:
                            identify::Info {
                                protocol_version,
                                agent_version,
                                listen_addrs,
                                protocols,
                                observed_addr,
                                ..
                            },
                    },
                )) => {
                    if peer_id == peer {
                        return Ok(Peer {
                            peer_id,
                            protocol_version,
                            agent_version,
                            listen_addrs,
                            protocols,
                            observed_addr,
                        });
                    } else {
                        println!("get identity from {}", peer_id);
                        // self.swarm.disconnect_peer_id(peer_id).unwrap();
                        if !self.swarm.is_connected(&peer) {
                            println!("no connection!");
                        }
                    }
                }
                e => debug!("{e:?}"),
            }
        }
    }
}

#[derive(Debug, Error)]
enum LookupError {
    #[error("Looking up the given peer timed out")]
    Timeout,
    #[error("Failed to dial peer {error}")]
    FailedToDialPeer { error: libp2p::swarm::DialError },
    #[error("Failed to find peer on DHT")]
    FailedToFindPeerOnDht,
}

#[derive(NetworkBehaviour)]
struct LookupBehaviour {
    pub(crate) kademlia: Kademlia<MemoryStore>,
    pub(crate) ping: ping::Behaviour,
    pub(crate) identify: identify::Behaviour,
    relay: relay::client::Behaviour,
    keep_alive: swarm::keep_alive::Behaviour,
}

arg_enum! {
    #[derive(Debug, Clone)]
    enum Network {
        Kusama,
        Polkadot,
        Ipfs,
        Ursa,
        Pactus,
    }
}

impl Network {
    #[rustfmt::skip]
    fn bootnodes(&self) -> Vec<(Multiaddr, PeerId)> {
        match self {
            Network::Kusama => {
                vec![
                    ("/dns/p2p.cc3-0.kusama.network/tcp/30100".parse().unwrap(), FromStr::from_str("12D3KooWDgtynm4S9M3m6ZZhXYu2RrWKdvkCSScc25xKDVSg1Sjd").unwrap()),
                    ("/dns/p2p.cc3-1.kusama.network/tcp/30100".parse().unwrap(), FromStr::from_str("12D3KooWNpGriWPmf621Lza9UWU9eLLBdCFaErf6d4HSK7Bcqnv4").unwrap()),
                    ("/dns/p2p.cc3-2.kusama.network/tcp/30100".parse().unwrap(), FromStr::from_str("12D3KooWLmLiB4AenmN2g2mHbhNXbUcNiGi99sAkSk1kAQedp8uE").unwrap()),
                    ("/dns/p2p.cc3-3.kusama.network/tcp/30100".parse().unwrap(), FromStr::from_str("12D3KooWEGHw84b4hfvXEfyq4XWEmWCbRGuHMHQMpby4BAtZ4xJf").unwrap()),
                    ("/dns/p2p.cc3-4.kusama.network/tcp/30100".parse().unwrap(), FromStr::from_str("12D3KooWF9KDPRMN8WpeyXhEeURZGP8Dmo7go1tDqi7hTYpxV9uW").unwrap()),
                    ("/dns/p2p.cc3-5.kusama.network/tcp/30100".parse().unwrap(), FromStr::from_str("12D3KooWDiwMeqzvgWNreS9sV1HW3pZv1PA7QGA7HUCo7FzN5gcA").unwrap()),
                    ("/dns/kusama-bootnode-0.paritytech.net/tcp/30333".parse().unwrap(), FromStr::from_str("12D3KooWSueCPH3puP2PcvqPJdNaDNF3jMZjtJtDiSy35pWrbt5h").unwrap()),
                    ("/dns/kusama-bootnode-1.paritytech.net/tcp/30333".parse().unwrap(), FromStr::from_str("12D3KooWQKqane1SqWJNWMQkbia9qiMWXkcHtAdfW5eVF8hbwEDw").unwrap())
                ]
            }
            Network::Polkadot => {
                vec![
                    // ("/dns/p2p.cc1-0.polkadot.network/tcp/30100".parse().unwrap(), FromStr::from_str("12D3KooWEdsXX9657ppNqqrRuaCHFvuNemasgU5msLDwSJ6WqsKc").unwrap()),
                    ("/dns/p2p.cc1-1.polkadot.network/tcp/30100".parse().unwrap(), FromStr::from_str("12D3KooWAtx477KzC8LwqLjWWUG6WF4Gqp2eNXmeqAG98ehAMWYH").unwrap()),
                    ("/dns/p2p.cc1-2.polkadot.network/tcp/30100".parse().unwrap(), FromStr::from_str("12D3KooWAGCCPZbr9UWGXPtBosTZo91Hb5M3hU8v6xbKgnC5LVao").unwrap()),
                    ("/dns/p2p.cc1-3.polkadot.network/tcp/30100".parse().unwrap(), FromStr::from_str("12D3KooWJ4eyPowiVcPU46pXuE2cDsiAmuBKXnFcFPapm4xKFdMJ").unwrap()),
                    ("/dns/p2p.cc1-4.polkadot.network/tcp/30100".parse().unwrap(), FromStr::from_str("12D3KooWNMUcqwSj38oEq1zHeGnWKmMvrCFnpMftw7JzjAtRj2rU").unwrap()),
                    ("/dns/p2p.cc1-5.polkadot.network/tcp/30100".parse().unwrap(), FromStr::from_str("12D3KooWDs6LnpmWDWgZyGtcLVr3E75CoBxzg1YZUPL5Bb1zz6fM").unwrap()),
                    ("/dns/cc1-0.parity.tech/tcp/30333".parse().unwrap(), FromStr::from_str("12D3KooWSz8r2WyCdsfWHgPyvD8GKQdJ1UAiRmrcrs8sQB3fe2KU").unwrap()),
                    ("/dns/cc1-1.parity.tech/tcp/30333".parse().unwrap(), FromStr::from_str("12D3KooWFN2mhgpkJsDBuNuE5427AcDrsib8EoqGMZmkxWwx3Md4").unwrap()),
                ]
            }
            Network::Ipfs => {
                vec![
                    ("/ip4/104.131.131.82/tcp/4001".parse().unwrap(), FromStr::from_str("QmaCpDMGvV2BGHeYERUEnRQAwe3N8SzbUtfsmvsqQLuvuJ").unwrap()),
                    ("/dnsaddr/bootstrap.libp2p.io".parse().unwrap(), FromStr::from_str("QmNnooDu7bfjPFoTZYxMNLWUQJyrVwtbZg5gBMjTezGAJN").unwrap()),
                    ("/dnsaddr/bootstrap.libp2p.io".parse().unwrap(), FromStr::from_str("QmQCU2EcMqAqQPR2i9bChDtGNJchTbq5TbXJJ16u19uLTa").unwrap()),
                    ("/dnsaddr/bootstrap.libp2p.io".parse().unwrap(), FromStr::from_str("QmbLHAnMoJPWSCR5Zhtx6BHJX9KiKNN6tpvbUcqanj75Nb").unwrap()),
                    ("/dnsaddr/bootstrap.libp2p.io".parse().unwrap(), FromStr::from_str("QmcZf59bWwK5XFi76CZX8cbJ4BhTzzA3gU1ZjYZcYW3dwt").unwrap()),
                ]
            }
            Network::Ursa => {
                vec![
                    ("/dns/bootstrap-node-0.ursa.earth/tcp/6009".parse().unwrap(), FromStr::from_str("12D3KooWDji7xMLia6GAsyr4oiEFD2dd3zSryqNhfxU3Grzs1r9p").unwrap()),
                ]
            }
            Network::Pactus => {
                vec![
                    ("/dns/bootstrap1.pactus.org/tcp/21888".parse().unwrap(), FromStr::from_str("12D3KooWMnDsu8TDTk2VV8uD8zsNSB6eUkqtQs6ttg4bHq9zNaBe").unwrap()), // ok
                    ("/dns/bootstrap2.pactus.org/tcp/21888".parse().unwrap(), FromStr::from_str("12D3KooWM39ag7ghta49qybf7McADgT8FLakTYkCsiBvwdnjuG5q").unwrap()), //ok
                    ("/dns/bootstrap3.pactus.org/tcp/21888".parse().unwrap(), FromStr::from_str("12D3KooWBCPSZWheet6tMoHbVBCDfBwQm4yzCwcQ8hJ6NMCN97sj").unwrap()), // ok
                    ("/dns/bootstrap4.pactus.org/tcp/21888".parse().unwrap(), FromStr::from_str("12D3KooWKg6aLa77yAaqMCb45aH5iQuTr5GzRUWUCJ1sZYB5vnoL").unwrap()), // ok
                    ("/dns/pactus-bootstrap.sensifia.com/tcp/21888".parse().unwrap(), FromStr::from_str("12D3KooWLpgBQt7A1W8FhTLVYwQMEnTBEX7uudLvd1YMZsDHqqWh").unwrap()),
                    ("/ip4/65.108.211.187/tcp/21888".parse().unwrap(), FromStr::from_str("12D3KooWGLrSiE5KWoc9GpXfryvKqheHj6j78ryMDDC17SY4YoCi").unwrap()), // ok
                    ("/ip4/84.247.165.249/tcp/21888".parse().unwrap(), FromStr::from_str("12D3KooWQmv2FcNQfh1EhA98twt8ePdkQaxEPeYfinEYyVS16juY").unwrap()), // Ok
                    ("/ip4/62.171.130.196/tcp/21888".parse().unwrap(), FromStr::from_str("12D3KooWNSiLJejwiG5Hk726TGyXPATFsYeKvarKgNpyjMiKupDH").unwrap()), // ok
                    ("/ip4/135.181.42.222/tcp/21888".parse().unwrap(), FromStr::from_str("12D3KooWN46UYQNoZvzd9GiJPQC7LByj6RAg8rRxdUTiQ24vHsN1").unwrap()), // ok
                    ("/dns/pactus-bootstrap.stakers.world/tcp/21888".parse().unwrap(), FromStr::from_str("12D3KooWK1DAcxf8RJA2PVkwr9ehk1zgXcnjLyPCsYCpvw1NSfhc").unwrap()), // ok
                    ("/dns/pactus-bootstrap.ionode.online/tcp/21888".parse().unwrap(), FromStr::from_str("12D3KooWG5tvvfS8PRd2Bgyxxqrok7QfohkHSprFPxmT3HTh8dVD").unwrap()), // ok
                ]
            }
        }
    }

    fn protocol(&self) -> Option<String> {
        match self {
            Network::Kusama => Some("/ksmcc3/kad".to_string()),
            Network::Polkadot => Some("/dot/kad".to_string()),
            Network::Ipfs => None,
            Network::Ursa => Some("/ursa/kad/0.0.1".to_string()),
            Network::Pactus => Some("/pactus/gossip/v1/kad/1.0.0".to_string()),
        }
    }
}
