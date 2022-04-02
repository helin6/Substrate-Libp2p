use sc_network::{block_request_handler::BlockRequestHandler, config, light_client_requests::handler::LightClientRequestHandler, state_request_handler::StateRequestHandler, Event, NetworkService, NetworkWorker};
use futures::prelude::*;
// use libp2p::PeerId;
use sp_runtime::traits::{Block as BlockT, Header as _};
use std::{borrow::Cow, sync::Arc, time::Duration};
// use sc_network::request_responses::ProtocolConfig;
use substrate_test_runtime_client::{TestClientBuilder, TestClientBuilderExt as _};

type TestNetworkService = NetworkService<
    substrate_test_runtime_client::runtime::Block,
    substrate_test_runtime_client::runtime::Hash,
>;


fn build_test_full_node(
    config: config::NetworkConfiguration,
) -> (Arc<TestNetworkService>, impl Stream<Item = Event>)  {
    let client = Arc::new(TestClientBuilder::with_default_backend().build_with_longest_chain().0);

    #[derive(Clone)]
    struct PassThroughVerifier(bool);

    #[async_trait::async_trait]
    impl<B: BlockT> sc_consensus::Verifier<B> for PassThroughVerifier {
        async fn verify(
            &mut self,
            mut block: sc_consensus::BlockImportParams<B, ()>,
        ) -> Result<
            (
                sc_consensus::BlockImportParams<B, ()>,
                Option<Vec<(sp_blockchain::well_known_cache_keys::Id, Vec<u8>)>>,
            ),
            String,
        > {
            let maybe_keys = block
                .header
                .digest()
                .log(|l| {
                    l.try_as_raw(sp_runtime::generic::OpaqueDigestItemId::Consensus(b"aura"))
                        .or_else(|| {
                            l.try_as_raw(sp_runtime::generic::OpaqueDigestItemId::Consensus(
                                b"babe",
                            ))
                        })
                })
                .map(|blob| {
                    vec![(sp_blockchain::well_known_cache_keys::AUTHORITIES, blob.to_vec())]
                });

            block.finalized = self.0;
            block.fork_choice = Some(sc_consensus::ForkChoiceStrategy::LongestChain);
            Ok((block, maybe_keys))
        }
    }

    let import_queue = Box::new(sc_consensus::BasicQueue::new(
        PassThroughVerifier(false),
        Box::new(client.clone()),
        None,
        &sp_core::testing::TaskExecutor::new(),
        None,
    ));

    let protocol_id = config::ProtocolId::from("/test-protocol-name");

    let block_request_protocol_config = {
        let (handler, protocol_config) = BlockRequestHandler::new(&protocol_id, client.clone(), 50);
        async_std::task::spawn(handler.run().boxed());
        protocol_config
    };

    let state_request_protocol_config = {
        let (handler, protocol_config) = StateRequestHandler::new(&protocol_id, client.clone(), 50);
        async_std::task::spawn(handler.run().boxed());
        protocol_config
    };

    let light_client_request_protocol_config = {
        let (handler, protocol_config) =
            LightClientRequestHandler::new(&protocol_id, client.clone());
        async_std::task::spawn(handler.run().boxed());
        protocol_config
    };

    let worker = NetworkWorker::new(config::Params {
        role: config::Role::Full,
        executor: None,
        transactions_handler_executor: Box::new(|task| {
            async_std::task::spawn(task);
        }),
        network_config: config,
        chain: client.clone(),
        transaction_pool: Arc::new(crate::config::EmptyTransactionPool),
        protocol_id,
        import_queue,
        block_announce_validator: Box::new(
            sp_consensus::block_validation::DefaultBlockAnnounceValidator,
        ),
        metrics_registry: None,
        block_request_protocol_config,
        state_request_protocol_config,
        light_client_request_protocol_config,
        warp_sync: None,
    })
        .unwrap();

    let service = worker.service().clone();
    let event_stream = service.event_stream("test");

    async_std::task::spawn(async move {
        futures::pin_mut!(worker);
        let _ = worker.await;
    });

    (service, event_stream)
}

const PROTOCOL_NAME: Cow<'static, str> = Cow::Borrowed("/foo");

/// Builds two nodes and their associated events stream.
/// The nodes are connected together and have the `PROTOCOL_NAME` protocol registered.
fn build_nodes_one_proto() -> (
    Arc<TestNetworkService>,
    impl Stream<Item = Event>,
    Arc<TestNetworkService>,
    impl Stream<Item = Event>
) {
    let listen_addr = config::build_multiaddr![Memory(rand::random::<u64>())];
    let (node1, events_stream1) = build_test_full_node(config::NetworkConfiguration {
        extra_sets: vec![config::NonDefaultSetConfig {
            notifications_protocol: PROTOCOL_NAME,
            fallback_names: Vec::new(),
            max_notification_size: 1024 * 1024,
            set_config: Default::default(),
        }],
        listen_addresses: vec![listen_addr.clone()],
        transport: config::TransportConfig::MemoryOnly,
        ..config::NetworkConfiguration::new_local()
    });

    let (node2, events_stream2) = build_test_full_node(config::NetworkConfiguration {
        extra_sets: vec![config::NonDefaultSetConfig {
            notifications_protocol: PROTOCOL_NAME,
            fallback_names: Vec::new(),
            max_notification_size: 1024 * 1024,
            set_config: config::SetConfig {
                reserved_nodes: vec![config::MultiaddrWithPeerId {
                    multiaddr: listen_addr,
                    peer_id: node1.local_peer_id().clone(),
                }],
                ..Default::default()
            },
        }],
        listen_addresses: vec![],
        transport: config::TransportConfig::MemoryOnly,
        ..config::NetworkConfiguration::new_local()
    });

    (node1, events_stream1, node2, events_stream2)
}

#[test]
fn notifications_state_consistent() {
    let (node1, mut events_stream1, node2, mut events_stream2) = build_nodes_one_proto();

    async_std::task::block_on(async move {
        // True if we have an active substream from node1 to node2.
        let mut node1_to_node2_open = false;
        // True if we have an active substream from node2 to node1.
        let mut node2_to_node1_open = false;
        // We stop the test after a certain number of iterations.
        let mut iterations = 0;
        // Safe guard because we don't want the test to pass if no substream has been open.

        loop {
            println!("iterations: {}", iterations);
            iterations += 1;
            if iterations >= 100 {
                // assert!(something_happened);
                break
            }

            // Start by sending a notification from node1 to node2 and vice-versa. Part of the
            // test consists in ensuring that notifications get ignored if the stream isn't open.
            if rand::random::<u8>() % 5 >= 3 {
                    node1.write_notification(
                        node2.local_peer_id().clone(),
                        PROTOCOL_NAME,
                        format!("hello #{}", iterations).into_bytes(),
                    );
                    println!("node1 write_notification: {:?}", format!("hello #{}", iterations));
            }
            if rand::random::<u8>() % 5 >= 3 {
                    node2.write_notification(
                        node1.local_peer_id().clone(),
                        PROTOCOL_NAME,
                        format!("hello #{}", iterations).into_bytes(),
                    );
                    println!("node2 write_notification: {:?}", format!("hello #{}", iterations));
            }

            // Also randomly disconnect the two nodes from time to time.
            // if rand::random::<u8>() % 20 == 0 {
            //     println!("node1 disconnect to node2");
            //     node1.disconnect_peer(node2.local_peer_id().clone(), PROTOCOL_NAME);
            // }
            // if rand::random::<u8>() % 20 == 0 {
            //     println!("node2 disconnect to node1");
            //     node2.disconnect_peer(node1.local_peer_id().clone(), PROTOCOL_NAME);
            // }


            let next_event = {
                let next1 = events_stream1.next();
                let next2 = events_stream2.next();
                // We also await on a small timer, otherwise it is possible for the test to wait
                // forever while nothing at all happens on the network.
                let continue_test = futures_timer::Delay::new(Duration::from_millis(200));
                match future::select(future::select(next1, next2), continue_test).await {
                    future::Either::Left((future::Either::Left((Some(ev), _)), _)) => { println!("get node1's event: {:?}", ev); future::Either::Left(ev) }
                    future::Either::Left((future::Either::Right((Some(ev), _)), _)) => { println!("get node2's event: {:?}", ev); future::Either::Right(ev) }
                    future::Either::Right(_) => { println!("not match any event"); continue },
                    _ => break,
                }
            };

            match next_event {
                future::Either::Left(Event::NotificationStreamOpened {
                                         remote, protocol, ..
                                     }) => {
                    println!("open node1 to node2 stream");
                    // something_happened = true;
                    if protocol == PROTOCOL_NAME {
                        assert!(!node1_to_node2_open);
                        node1_to_node2_open = true;
                        assert_eq!(remote, *node2.local_peer_id());
                    }
                    // assert_eq!(protocol, PROTOCOL_NAME);
                },
                future::Either::Right(Event::NotificationStreamOpened {
                                          remote, protocol, ..
                                      }) => {
                    println!("open node2 to node1 stream");
                    if protocol == PROTOCOL_NAME {
                        assert!(!node2_to_node1_open);
                        node2_to_node1_open = true;
                        assert_eq!(remote, *node1.local_peer_id());
                    }
                    // assert_eq!(protocol, PROTOCOL_NAME);
                },
                future::Either::Left(Event::NotificationStreamClosed {
                                         remote: _, protocol: _, ..
                                     }) => {
                    println!("close node1 to node2 stream");
                    // assert!(node1_to_node2_open);
                    // node1_to_node2_open = false;
                    // assert_eq!(remote, *node2.local_peer_id());
                    // assert_eq!(protocol, PROTOCOL_NAME);
                },
                future::Either::Right(Event::NotificationStreamClosed {
                                          remote: _, protocol: _, ..
                                      }) => {
                    println!("close node2 to node1 stream");
                    // assert!(node2_to_node1_open);
                    // node2_to_node1_open = false;
                    // assert_eq!(remote, *node1.local_peer_id());
                    // assert_eq!(protocol, PROTOCOL_NAME);
                },
                future::Either::Left(Event::NotificationsReceived { remote, messages }) => {
                    // assert!(node1_to_node2_open);
                    assert_eq!(remote, *node2.local_peer_id());
                    println!("node1 receive msg: {:?}", messages[0].1);
                    if rand::random::<u8>() % 5 >= 4 {
                        println!("node1 send to node2: hello world");
                        node1.write_notification(
                            node2.local_peer_id().clone(),
                            PROTOCOL_NAME,
                            b"hello world".to_vec(),
                        );
                    }
                },
                future::Either::Right(Event::NotificationsReceived { remote, messages }) => {
                    // assert!(node2_to_node1_open);
                    assert_eq!(remote, *node1.local_peer_id());
                    println!("node2 receive msg: {:?}", messages[0].1);
                    if rand::random::<u8>() % 5 >= 4 {
                        println!("node2 send to node1: hello world");
                        node2.write_notification(
                            node1.local_peer_id().clone(),
                            PROTOCOL_NAME,
                            b"hello world".to_vec(),
                        );
                    }
                },

                // Add new events here.
                future::Either::Left(Event::SyncConnected { .. }) => {},
                future::Either::Right(Event::SyncConnected { .. }) => {},
                future::Either::Left(Event::SyncDisconnected { .. }) => {},
                future::Either::Right(Event::SyncDisconnected { .. }) => {},
                future::Either::Left(Event::Dht(_)) => {},
                future::Either::Right(Event::Dht(_)) => {},
            };
        }
    });
}

#[test]
fn pressure_test() {
    let(node1, mut events_stream1, node2, mut events_stream2) = build_nodes_one_proto();

    //test pressure, send msgs from node1 to node2
    //receiver start work after stream opened
    let receiver = async_std::task::spawn(async move {
        let mut received_notifications = 0;

        while received_notifications < 100 {
            match events_stream2.next().await.unwrap() {
                Event::NotificationStreamClosed { .. } => panic!(),
                Event::NotificationsReceived { messages, .. } =>
                    for message in messages {
                        println!("receive message: {:?}", message);
                        assert_eq!(message.0, PROTOCOL_NAME);
                        assert_eq!(message.1, format!("hello #{}", received_notifications));
                        received_notifications += 1;
                    },
                _ => {},
            };

            // if rand::random::<u8>() < 2 {
            //     async_std::task::sleep(Duration::from_millis(500)).await;
            // }
        }
    });

    async_std::task::block_on(async move {
        // Break loof after stream opened
        loop {
            match events_stream1.next().await.unwrap() {
                Event::NotificationStreamOpened { .. } => break,
                _ => {},
            };
        }

        // Sending!
        for num in 0..100 {
            let notif = node1.notification_sender(node2.local_peer_id().clone(), PROTOCOL_NAME).unwrap();
            notif.ready().await.unwrap().send(format!("hello #{}", num)).unwrap();
            println!("send msg: {:?}", format!("hello #{}", num));
        }

        receiver.await;
    });
}
