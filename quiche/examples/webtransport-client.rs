// Copyright (C) 2019, Cloudflare, Inc.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//     * Redistributions of source code must retain the above copyright notice,
//       this list of conditions and the following disclaimer.
//
//     * Redistributions in binary form must reproduce the above copyright
//       notice, this list of conditions and the following disclaimer in the
//       documentation and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
// IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO,
// THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
// PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
// EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
// PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
// PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
// NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

#[macro_use]
extern crate log;

use std::net::ToSocketAddrs;

use quiche::h3::{webtransport::ClientSession, NameValue};

use ring::rand::*;

const MAX_DATAGRAM_SIZE: usize = 1350;

fn main() {
    let mut buf = [0; 65535];
    let mut out = [0; MAX_DATAGRAM_SIZE];

    let mut args = std::env::args();

    let cmd = &args.next().unwrap();

    if args.len() != 1 {
        println!("Usage: {cmd} URL");
        println!("\nSee tools/apps/ for more complete implementations.");
        return;
    }

    let url = url::Url::parse(&args.next().unwrap()).unwrap();

    // Setup the event loop.
    let mut poll = mio::Poll::new().unwrap();
    let mut events = mio::Events::with_capacity(1024);

    // Resolve server address.
    let peer_addr = url.to_socket_addrs().unwrap().next().unwrap();

    // Bind to INADDR_ANY or IN6ADDR_ANY depending on the IP family of the
    // server address. This is needed on macOS and BSD variants that don't
    // support binding to IN6ADDR_ANY for both v4 and v6.
    let bind_addr = match peer_addr {
        std::net::SocketAddr::V4(_) => "0.0.0.0:0",
        std::net::SocketAddr::V6(_) => "[::]:0",
    };

    // Create the UDP socket backing the QUIC connection, and register it with
    // the event loop.
    let mut socket =
        mio::net::UdpSocket::bind(bind_addr.parse().unwrap()).unwrap();
    poll.registry()
        .register(&mut socket, mio::Token(0), mio::Interest::READABLE)
        .unwrap();

    // Create the configuration for the QUIC connection.
    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();

    // *CAUTION*: this should not be set to `false` in production!!!
    config.verify_peer(false);

    config
        .set_application_protos(quiche::h3::APPLICATION_PROTOCOL)
        .unwrap();

    config.set_max_idle_timeout(5000);
    config.set_max_recv_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_max_send_udp_payload_size(MAX_DATAGRAM_SIZE);
    config.set_initial_max_data(10_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_stream_data_uni(1_000_000);
    config.set_initial_max_streams_bidi(100);
    config.set_initial_max_streams_uni(100);
    config.set_disable_active_migration(true);
    config.enable_dgram(true, 3, 3);

    let mut client_session = None;

    // Generate a random source connection ID for the connection.
    let mut scid = [0; quiche::MAX_CONN_ID_LEN];
    SystemRandom::new().fill(&mut scid[..]).unwrap();

    let scid = quiche::ConnectionId::from_ref(&scid);

    // Get local address.
    let local_addr = socket.local_addr().unwrap();

    // Create a QUIC connection and initiate handshake.
    let mut conn =
        quiche::connect(url.domain(), &scid, local_addr, peer_addr, &mut config)
            .unwrap();

    info!(
        "connecting to {:} from {:} with scid {}",
        peer_addr,
        socket.local_addr().unwrap(),
        hex_dump(&scid)
    );

    let (write, send_info) = conn.send(&mut out).expect("initial send failed");

    while let Err(e) = socket.send_to(&out[..write], send_info.to) {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            debug!("send() would block");
            continue;
        }

        panic!("send() failed: {:?}", e);
    }

    debug!("written {}", write);

    let mut h3_config = quiche::h3::Config::new().unwrap();
    h3_config.set_enable_webtransport(true);

    // Prepare request.
    let mut path = String::from(url.path());
    let authority = url.host_str().unwrap().as_bytes();

    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }

    loop {
        poll.poll(&mut events, conn.timeout()).unwrap();

        // Read incoming UDP packets from the socket and feed them to quiche,
        // until there are no more packets to read.
        'read: loop {
            // If the event loop reported no events, it means that the timeout
            // has expired, so handle it without attempting to read packets. We
            // will then proceed with the send loop.
            if events.is_empty() {
                debug!("timed out");

                conn.on_timeout();

                break 'read;
            }

            let (len, from) = match socket.recv_from(&mut buf) {
                Ok(v) => v,

                Err(e) => {
                    // There are no more UDP packets to read, so end the read
                    // loop.
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        debug!("recv() would block");
                        break 'read;
                    }

                    panic!("recv() failed: {:?}", e);
                },
            };

            debug!("got {} bytes", len);

            let recv_info = quiche::RecvInfo {
                to: local_addr,
                from,
            };

            // Process potentially coalesced packets.
            let read = match conn.recv(&mut buf[..len], recv_info) {
                Ok(v) => v,

                Err(e) => {
                    error!("recv failed: {:?}", e);
                    continue 'read;
                },
            };

            debug!("processed {} bytes", read);
        }

        debug!("done reading");

        if conn.is_closed() {
            info!("connection closed, {:?}", conn.stats());
            break;
        }

        // Create a new HTTP/3 connection once the QUIC connection is established.
        if conn.is_established() && client_session.is_none() {
            client_session = Some(
                ClientSession::with_transport(&mut conn)
                    .expect("unable to create client session"),
            );
        }

        if let Some(client_session) = &mut client_session {
            // Process WebTrasnport events.
            loop {
                match client_session.poll(&mut conn) {
                    Ok(quiche::h3::webtransport::ClientEvent::PeerReady) => {
                        // This event is issued when a SETTINGS frame from the server is received and
                        // it detects that WebTransport is enabled.
                        match client_session.send_connect_request(
                            &mut conn,
                            authority,
                            path.as_bytes(),
                            authority,
                            None,
                        ){
                            Ok(stream_id) => stream_id,
                            Err(_) => 0,
                        };
                    },
                    Ok(quiche::h3::webtransport::ClientEvent::Connected) => {
                        // receive response from server for CONNECT request,
                        // and it indicates server accepted it.
                        // you can start to send any data through Stream or send Datagram
                    },
                    Ok(quiche::h3::webtransport::ClientEvent::Rejected(code)) => {
                        // receive response from server for CONNECT request,
                        // and it indicates server rejected it.
                        // you may want to close session.
                    },
                    Ok(quiche::h3::webtransport::ClientEvent::StreamData(
                        stream_id,
                    )) => {
                        let mut buf = vec![0; 10000];
                        while let Ok(len) = client_session
                            .recv_stream_data(&mut conn, stream_id, &mut buf)
                        {
                            let stream_data = &buf[0..len];
                            //handle_stream_data(stream_data);
                        }
                    },
                    Ok(quiche::h3::webtransport::ClientEvent::Datagram) => {
                        let mut buf = vec![0; 1500];
                        while let Ok((in_session, offset, total)) =
                            client_session.recv_dgram(&mut conn, &mut buf)
                        {
                            if in_session {
                                let dgram = &buf[offset..total];
                                // handle_dgram(dgram);
                            } else {
                                // this dgram is not related to current WebTransport session. ignore.
                            }
                        }
                    },
                    Ok(
                        quiche::h3::webtransport::ClientEvent::StreamFinished(
                            stream_id,
                        ),
                    ) => {
                        // A WebTrnasport stream finished, handle it.
                    },
                    Ok(
                        quiche::h3::webtransport::ClientEvent::SessionFinished,
                    ) => {
                        // Peer finish session stream, handle it.
                    },
                    Ok(quiche::h3::webtransport::ClientEvent::SessionReset(
                        e,
                    )) => {
                        // Peer reset session stream, handle it.
                    },
                    Ok(quiche::h3::webtransport::ClientEvent::SessionGoAway) => {
                        // Peer signalled it is going away, handle it.
                    },
                    Ok(quiche::h3::webtransport::ClientEvent::Other(
                        stream_id,
                        event,
                    )) => {
                        // Original h3::Event which is not related to WebTransport.
                    },
                    Err(quiche::h3::webtransport::Error::Done) => break,
                    Err(e) => break,
                }
            }
        }

        // Generate outgoing QUIC packets and send them on the UDP socket, until
        // quiche reports that there are no more packets to be sent.
        loop {
            let (write, send_info) = match conn.send(&mut out) {
                Ok(v) => v,

                Err(quiche::Error::Done) => {
                    debug!("done writing");
                    break;
                },

                Err(e) => {
                    error!("send failed: {:?}", e);

                    conn.close(false, 0x1, b"fail").ok();
                    break;
                },
            };

            if let Err(e) = socket.send_to(&out[..write], send_info.to) {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    debug!("send() would block");
                    break;
                }

                panic!("send() failed: {:?}", e);
            }

            debug!("written {}", write);
        }

        if conn.is_closed() {
            info!("connection closed, {:?}", conn.stats());
            break;
        }
    }
}

fn hex_dump(buf: &[u8]) -> String {
    let vec: Vec<String> = buf.iter().map(|b| format!("{b:02x}")).collect();

    vec.join("")
}

pub fn hdrs_to_strings(hdrs: &[quiche::h3::Header]) -> Vec<(String, String)> {
    hdrs.iter()
        .map(|h| {
            let name = String::from_utf8_lossy(h.name()).to_string();
            let value = String::from_utf8_lossy(h.value()).to_string();

            (name, value)
        })
        .collect()
}
