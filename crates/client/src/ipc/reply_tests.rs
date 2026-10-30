use super::*;
use gmux_protocol::{
    control::{Command, CommandOutput, CommandReply, TabCommand},
    model::{DaemonId, MachineKind},
    network::{ConnectionInfo, Handshake, read_json, write_json},
};
use serde_json::json;
use std::{io::BufReader, net::TcpListener, os::unix::net::UnixListener};

fn snapshot(daemon: DaemonId) -> CommandReply {
    CommandReply {
        daemon,
        home_directory: "/home/test".into(),
        machine_kind: MachineKind::Server,
        hostname: "test".into(),
        revision: 17,
        result: gmux_protocol::control::CommandResult {
            output: CommandOutput::Split { pane: 7 },
            warning: Some("Could not save execution".into()),
        },
        execution: Default::default(),
        metadata: vec![],
    }
}

/// Exercise the real framing and endpoint APIs with a controlled socket peer.
fn peer(tcp: bool, replies: Vec<Value>) -> (Endpoint, std::thread::JoinHandle<()>) {
    let daemon = DaemonId::from_bytes([1; 16]);
    if tcp {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = Endpoint::tcp(
            listener.local_addr().unwrap().to_string(),
            Default::default(),
            Some(daemon),
        );
        let server = std::thread::spawn(move || {
            for reply in replies {
                let (mut socket, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let handshake: Handshake =
                    serde_json::from_value(read_json(&mut reader, 4096).unwrap()).unwrap();
                assert_eq!(handshake.daemon, Some(daemon));
                write_json(
                    &mut socket,
                    &ConnectionInfo {
                        version: gmux_protocol::transport::VERSION,
                        daemon,
                        home_directory: "/home/test".into(),
                        machine_kind: MachineKind::Server,
                        hostname: "test".into(),
                    },
                )
                .unwrap();
                let request = read_json(&mut reader, 4096).unwrap();
                assert_ne!(request["method"], "_execution");
                write_json(&mut socket, &reply).unwrap();
            }
        });
        (endpoint, server)
    } else {
        let path = std::env::temp_dir().join(format!(
            "gmux-reply-{}.sock",
            gmux_platform::unix::identity::daemon_id().unwrap()
        ));
        let listener = UnixListener::bind(&path).unwrap();
        let endpoint = Endpoint::unix(path.clone(), companion_path(".stream"));
        let server = std::thread::spawn(move || {
            for reply in replies {
                let (mut socket, _) = listener.accept().unwrap();
                read_json(&mut BufReader::new(socket.try_clone().unwrap()), 4096).unwrap();
                write_json(&mut socket, &reply).unwrap();
            }
            std::fs::remove_file(path).unwrap();
        });
        (endpoint, server)
    }
}

#[test]
fn sync_async_and_queued_execution_preserve_payloads_and_errors_on_both_transports() {
    for tcp in [false, true] {
        let daemon = DaemonId::from_bytes([1; 16]);
        let replies = vec![
            serde_json::to_value(snapshot(daemon)).unwrap(),
            json!({"text":"hello\n", "html":"<pre>hello</pre>"}),
            json!({"text":""}),
            json!({"ok":false}),
            json!({"busy":false}),
            json!({"error":"Pane 7 does not exist", "busy":true}),
            json!({"ok":true}), // Malformed busy reply must not silently become true.
            json!({"ok":true, "warning":"Could not capture scrollback"}),
        ];
        let (endpoint, server) = peer(tcp, replies);
        let reply = endpoint
            .command(
                Command::Tab(TabCommand::Split {
                    pane: Some(7),
                    down: false,
                }),
                None,
            )
            .unwrap();
        assert_eq!(reply.revision, 17);
        assert_eq!(reply.daemon, daemon);
        assert_eq!(
            reply.result.warning.as_deref(),
            Some("Could not save execution")
        );
        assert!(matches!(
            reply.result.output,
            CommandOutput::Split { pane: 7 }
        ));
        runtime::block_on(async {
            let reply = endpoint
                .execute_async(
                    &ExecutionRequest::CommandOutput {
                        pane: 7,
                        point: None,
                        formats: gmux_protocol::clipboard::Formats::TextHtml,
                    },
                    None,
                )
                .await
                .unwrap();
            let ExecutionReply::Content(content) = reply else {
                panic!("Expected clipboard content")
            };
            assert_eq!(content.text, "hello\n");
            assert_eq!(content.html.as_deref(), Some("<pre>hello</pre>"));
        });
        let reply = endpoint
            .execute(
                &ExecutionRequest::Copy {
                    pane: 7,
                    formats: Default::default(),
                },
                None,
            )
            .unwrap();
        assert!(
            matches!(reply, ExecutionReply::Content(content) if content.text.is_empty() && content.html.is_none())
        );
        let reply = endpoint
            .execute(
                &ExecutionRequest::Send {
                    pane: 7,
                    text: "input".into(),
                    paste: false,
                    enter: false,
                },
                None,
            )
            .unwrap();
        assert!(matches!(reply, ExecutionReply::Done(reply) if !reply.ok));
        let mut client = endpoint.client(Wake::new().unwrap());
        let ids: Vec<_> = (0..3)
            .map(|_| {
                client
                    .request(gmux_protocol::control::InternalRequest::Busy { panes: vec![7] })
                    .unwrap()
            })
            .collect();
        client.join();
        let mut replies = client.replies();
        let (id, reply) = replies.next().unwrap();
        assert_eq!(id, ids[0]);
        assert!(!reply.unwrap().into_busy().unwrap());
        let (id, reply) = replies.next().unwrap();
        assert_eq!(id, ids[1]);
        assert_eq!(reply.err().unwrap().to_string(), "Pane 7 does not exist");
        let (id, reply) = replies.next().unwrap();
        assert_eq!(id, ids[2]);
        assert!(reply.is_err());
        let reply = if tcp {
            endpoint.stop(None)
        } else {
            endpoint
                .execute(&ExecutionRequest::Stop, None)
                .unwrap()
                .into_stopped()
        }
        .unwrap();
        assert_eq!(
            reply.warning.as_deref(),
            Some("Could not capture scrollback")
        );
        server.join().unwrap();
    }
}

#[test]
fn command_outcomes_keep_warnings_and_rpc_json_keeps_qualified_identity() {
    for tcp in [false, true] {
        let daemon = DaemonId::from_bytes([1; 16]);
        let outcome = json!({"daemon":daemon,"pane":7,"warning":"Could not save execution"});
        let wire = if tcp {
            serde_json::to_value(snapshot(daemon)).unwrap()
        } else {
            outcome.clone()
        };
        let (endpoint, server) = peer(
            tcp,
            vec![wire.clone(), wire, json!({"error":"Cannot split"})],
        );
        let command = || {
            Command::Tab(TabCommand::Split {
                pane: Some(7),
                down: false,
            })
        };
        let reply = runtime::block_on(endpoint.command_result_async(command(), None)).unwrap();
        assert_eq!(reply.daemon, daemon);
        assert_eq!(
            reply.result.warning.as_deref(),
            Some("Could not save execution")
        );
        assert_eq!(endpoint.call(command()).unwrap(), outcome);
        assert_eq!(
            endpoint
                .command_result(command(), None)
                .err()
                .unwrap()
                .to_string(),
            "Cannot split"
        );
        server.join().unwrap();
    }
}

#[test]
fn tcp_reply_identity_is_checked_after_the_authenticated_handshake() {
    let wrong = DaemonId::from_bytes([2; 16]);
    let (endpoint, server) = peer(
        true,
        vec![
            serde_json::to_value(snapshot(wrong)).unwrap(),
            json!({"daemon":wrong,"tabs":[]}),
            json!({"daemon":wrong,"diagnostics":{}}),
            json!({"daemon":wrong,"ok":true}),
            json!({"daemon":wrong,"text":"clipboard"}),
        ],
    );
    for request in [
        ExecutionRequest::State,
        ExecutionRequest::List,
        ExecutionRequest::Status,
        ExecutionRequest::Notify {
            pane: 7,
            message: "hello".into(),
        },
        ExecutionRequest::Copy {
            pane: 7,
            formats: Default::default(),
        },
    ] {
        assert_eq!(
            endpoint.execute(&request, None).err().unwrap().to_string(),
            "Remote connection identity changed"
        );
    }
    server.join().unwrap();
}

#[test]
fn canceled_commands_do_not_resolve_or_connect_to_saved_endpoints() {
    let endpoint = Endpoint::remote("missing connection".into(), None);
    let cancellation = Cancellation::default();
    cancellation.cancel();
    let command = || {
        Command::Tab(TabCommand::Split {
            pane: Some(7),
            down: false,
        })
    };
    assert_eq!(
        endpoint
            .command(command(), Some(&cancellation))
            .err()
            .unwrap()
            .to_string(),
        "Request canceled"
    );
    assert_eq!(
        runtime::block_on(endpoint.command_result_async(command(), Some(&cancellation)))
            .err()
            .unwrap()
            .to_string(),
        "Request canceled"
    );
}

#[test]
fn unix_control_replies_do_not_require_resolving_implicit_targets() {
    let daemon = DaemonId::from_bytes([1; 16]);
    let (endpoint, server) = peer(
        false,
        vec![
            serde_json::to_value(snapshot(daemon)).unwrap(),
            json!({"text":"implicit target"}),
            json!({"text":"public target"}),
            json!({"error":"Daemon rejected the directory"}),
        ],
    );
    endpoint
        .command(
            Command::Tab(TabCommand::Split {
                pane: None,
                down: false,
            }),
            None,
        )
        .unwrap();
    let mut client = endpoint.client(Wake::new().unwrap());
    client
        .request(gmux_protocol::control::InternalRequest::Read { pane: None })
        .unwrap();
    client
        .request(gmux_protocol::control::PaneRequest::Read { pane: None })
        .unwrap();
    client
        .request(gmux_protocol::control::InternalRequest::Execution {
            request: ExecutionRequest::New {
                directory: Some("relative".into()),
                name: None,
            },
        })
        .unwrap();
    client.join();
    let mut replies = client.replies();
    assert!(
        matches!(replies.next().unwrap().1.unwrap(), ExecutionReply::Content(content) if content.text == "implicit target")
    );
    assert!(
        matches!(replies.next().unwrap().1.unwrap(), ExecutionReply::Content(content) if content.text == "public target")
    );
    assert_eq!(
        replies.next().unwrap().1.err().unwrap().to_string(),
        "Daemon rejected the directory"
    );
    server.join().unwrap();
}
