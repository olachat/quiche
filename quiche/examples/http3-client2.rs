#[macro_use]
extern crate log;

use std::net::ToSocketAddrs;

use log::{debug, error, info};
use mio;
use quiche::h3::NameValue;
use quiche::{self, ConnectionId, RecvInfo};
use ring::rand::{SecureRandom, SystemRandom};
use std::io;
use std::time::Instant;
use url::Url;

use ring::rand::*;

const MAX_DATAGRAM_SIZE: usize = 1350;

pub struct Http3Client {
    url: Url,
    peer_addr: std::net::SocketAddr,
    socket: mio::net::UdpSocket,
    poll: mio::Poll,
    config: quiche::Config,
    conn: Option<quiche::Connection>,
    http3_conn: Option<quiche::h3::Connection>,
    buf: [u8; MAX_DATAGRAM_SIZE],
    out: [u8; MAX_DATAGRAM_SIZE],
    events: mio::Events,
    req_sent: bool,
}

impl Http3Client {
    pub fn new(url: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let url = Url::parse(url)?;

        let mut poll = mio::Poll::new()?;
        let mut events = mio::Events::with_capacity(1024);

        let peer_addr = url.to_socket_addrs()?.next().unwrap();

        let bind_addr = match peer_addr {
            std::net::SocketAddr::V4(_) => "0.0.0.0:0",
            std::net::SocketAddr::V6(_) => "[::]:0",
        };

        let mut socket = mio::net::UdpSocket::bind(bind_addr.parse()?)?;
        poll.registry().register(
            &mut socket,
            mio::Token(0),
            mio::Interest::READABLE,
        )?;

        let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION)?;

        // *CAUTION*: this should not be set to `false` in production!!!
        config.verify_peer(false);

        config.set_application_protos(quiche::h3::APPLICATION_PROTOCOL)?;
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

        Ok(Http3Client {
            url,
            peer_addr,
            socket,
            poll,
            config,
            conn: None,
            http3_conn: None,
            buf: [0; MAX_DATAGRAM_SIZE],
            out: [0; MAX_DATAGRAM_SIZE],
            events,
            req_sent: false,
        })
    }

    pub fn connect(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let mut scid = [0; quiche::MAX_CONN_ID_LEN];
        SystemRandom::new().fill(&mut scid[..])?;
        let scid = quiche::ConnectionId::from_ref(&scid);

        let local_addr = self.socket.local_addr()?;

        self.conn = Some(quiche::connect(
            self.url.domain(),
            &scid,
            local_addr,
            self.peer_addr,
            &mut self.config,
        )?);
        info!(
            "connecting to {:} from {:} with scid {}",
            self.peer_addr,
            self.socket.local_addr()?,
            hex_dump(&scid)
        );

        let (write, send_info) = self
            .conn
            .as_mut()
            .unwrap()
            .send(&mut self.out)
            .expect("initial send failed");

        while let Err(e) = self.socket.send_to(&self.out[..write], send_info.to) {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                debug!("send() would block");
                continue;
            }

            panic!("send() failed: {:?}", e);
        }

        debug!("written {}", write);
        Ok(())
    }

    pub fn event_loop<F: FnMut(usize, &[u8])>(
        &mut self,
        on_data: &mut F,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let h3_config = quiche::h3::Config::new()?;

        let mut path = String::from(self.url.path());

        if let Some(query) = self.url.query() {
            path.push('?');
            path.push_str(query);
        }

        let req = vec![
            quiche::h3::Header::new(b":method", b"GET"),
            quiche::h3::Header::new(b":scheme", self.url.scheme().as_bytes()),
            quiche::h3::Header::new(
                b":authority",
                self.url.host_str().unwrap().as_bytes(),
            ),
            quiche::h3::Header::new(b":path", path.as_bytes()),
            quiche::h3::Header::new(b"user-agent", b"quiche"),
        ];

        let req_start = Instant::now();

        loop {
            self.poll
                .poll(&mut self.events, self.conn.as_ref().unwrap().timeout())?;

            self.handle_recv()?;
            self.handle_send()?;
            self.handle_http3_events(on_data)?;

            if self.conn.as_ref().unwrap().is_closed() {
                info!(
                    "connection closed, {:?}",
                    self.conn.as_ref().unwrap().stats()
                );
                break;
            }
        }

        Ok(())
    }

    fn handle_recv(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        'read: loop {
            if self.events.is_empty() {
                debug!("timed out");
    
                self.conn.as_mut().unwrap().on_timeout();
    
                break 'read;
            }
    
            let (len, from) = match self.socket.recv_from(&mut self.buf) {
                Ok(v) => v,
    
                Err(e) => {
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        debug!("recv() would block");
                        break 'read;
                    }
    
                    panic!("recv() failed: {:?}", e);
                },
            };
    
            debug!("got {} bytes", len);
    
            let recv_info = quiche::RecvInfo {
                to: self.local_addr,
                from,
            };
    
            let read = match self.conn.as_mut().unwrap().recv(&mut self.buf[..len], recv_info) {
                Ok(v) => v,
    
                Err(e) => {
                    error!("recv failed: {:?}", e);
                    continue 'read;
                },
            };
    
            debug!("processed {} bytes", read);
        }
    
        debug!("done reading");
        Ok(())
    }
    
    fn handle_send(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            let (write, send_info) = match self.conn.as_mut().unwrap().send(&mut self.out) {
                Ok(v) => v,
    
                Err(quiche::Error::Done) => {
                    debug!("done writing");
                    break;
                },
    
                Err(e) => {
                    error!("send failed: {:?}", e);
    
                    self.conn.as_mut().unwrap().close(false, 0x1, b"fail").ok();
                    break;
                },
            };
    
            if let Err(e) = self.socket.send_to(&self.out[..write], send_info.to) {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    debug!("send() would block");
                    break;
                }
    
                panic!("send() failed: {:?}", e);
            }
    
            debug!("written {}", write);
        }
    
        Ok(())
    }
    
    fn handle_http3_events<F: FnMut(usize, &[u8])>(
        &mut self,
        on_data: &mut F,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.conn.as_ref().unwrap().is_established() && self.http3_conn.is_none() {
            self.http3_conn = Some(
                quiche::h3::Connection::with_transport(
                    self.conn.as_mut().unwrap(),
                    &self.h3_config,
                )
                .expect("Unable to create HTTP/3 connection, check the server's uni stream limit and window size"),
            );
        }
    
        if let Some(h3_conn) = &mut self.http3_conn {
            if !self.req_sent {
                info!("sending HTTP request {:?}", self.req);
    
                h3_conn.send_request(self.conn.as_mut().unwrap(), &self.req, true)?;
    
                self.req_sent = true;
            }
        }
    
        if let Some(http3_conn) = &mut self.http3_conn {
            loop {
                match http3_conn.poll(self.conn.as_mut().unwrap()) {
    
                    Ok((stream_id, quiche::h3::Event::Data)) => {
                        while let Ok(read) =
                            http3_conn.recv_body(self.conn.as_mut().unwrap(), stream_id, &mut self.buf)
                        {
                            debug!(
                                "got {} bytes of response data on stream {}",
                                read, stream_id
                            );
                            on_data(stream_id, &self.buf[..read]);
                            }
                        },
            
                        Ok((stream_id, quiche::h3::Event::Headers { list, .. })) => {
                            info!(
                                "got response headers {:?} on stream id {}",
                                hdrs_to_strings(&list),
                                stream_id
                            );
                        },
    
                        Ok((_stream_id, quiche::h3::Event::WebTransportStreamData(_session_id))) => {},
        
                        Ok((_stream_id, quiche::h3::Event::Finished)) => {
                            info!(
                                "response received in {:?}, closing...",
                                req_start.elapsed()
                            );
    
                            conn.close(true, 0x100, b"kthxbye").unwrap();
                        },
    
                        Ok((_stream_id, quiche::h3::Event::Reset(e))) => {
                            error!(
                                "request was reset by peer with {}, closing...",
                                e
                            );
    
                            conn.close(true, 0x100, b"kthxbye").unwrap();
                        },
    
                        Ok((_flow_id, quiche::h3::Event::Datagram)) => (),
    
                        Ok((_, quiche::h3::Event::PriorityUpdate)) => unreachable!(),
    
                        Ok((goaway_id, quiche::h3::Event::GoAway)) => {
                            info!("GOAWAY id={}", goaway_id);
                        },
    
                        Err(quiche::h3::Error::Done) => {
                            break;
                        },
    
                        Err(e) => {
                            error!("HTTP/3 processing failed: {:?}", e);
    
                            break;
                        },
    
                    }
                }
            }
            
            Ok(())
        }
            
    
    
}


fn main() {
    let url = "https://example.com";
    let mut http3_client = Http3Client::new(url).unwrap();

    http3_client.connect().unwrap();

    http3_client
        .event_loop(&mut |stream_id, data| {
            println!("Stream {}: {}", stream_id, String::from_utf8_lossy(data));
        })
        .unwrap();
}