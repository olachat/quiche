use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use futures::{AsyncReadExt, AsyncWriteExt};
use quiche::{
    Config, Connection, ConnectionId, ConnectionIdLen, QuicVersion, StreamType, TransportError,
    MAX_CID_SIZE,
};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, Mutex};

struct WebTransportClient {
    connection: Option<Arc<Mutex<Connection>>>,
    sender: Option<mpsc::UnboundedSender<Vec<u8>>>,
    receiver: mpsc::UnboundedReceiver<Vec<u8>>,
    state: ClientState,
}

enum ClientState {
    Initialized,
    Connected,
    Closed,
}

impl WebTransportClient {
    pub async fn connect(&mut self, server_addr: SocketAddr) -> Result<()> {
        let local_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

        let udp_socket = UdpSocket::bind(local_addr).await?;
        udp_socket.connect(server_addr).await?;

        let mut config = Config::new(QuicVersion::default())?;
        config.verify_peer(false);

        let conn = Connection::new(
            &self.generate_connection_id(),
            server_addr,
            &config,
        )?;

        self.connection = Some(Arc::new(Mutex::new(conn)));

        self.state = ClientState::Connected;

        let (sender, receiver) = mpsc::unbounded_channel();

        self.sender = Some(sender);
        self.receiver = receiver;

        self.spawn_read_task();

        Ok(())
    }

    fn generate_connection_id(&self) -> ConnectionId {
        let mut cid = [0; MAX_CID_SIZE];
        let cid_len = ConnectionIdLen::Random;
        ConnectionId::new(&mut cid, cid_len)
    }

    pub async fn send_data(&self, data: Vec<u8>) -> Result<()> {
        if let Some(sender) = &self.sender {
            sender.send(data)?;
        } else {
            return Err(anyhow!("No sender channel found."));
        }

        Ok(())
    }

    pub fn on_data_received(&self, data: Vec<u8>) {
        println!("Received data: {:?}", data);
    }

    pub fn on_close(&mut self) {
        self.state = ClientState::Closed;
    }

    fn spawn_read_task(&self) {
        let conn = self.connection.clone().unwrap();

        let receiver = self.receiver.clone();

        let mut conn_clone = conn.lock().unwrap();

        tokio::spawn(async move {
            loop {
                let mut buf = [0; 65535];

                let recv_len = match receiver.recv().await {
                    Some(data) => data.len(),
                    None => 0,
                };

                if let Err(e) = conn_clone.send_stream_data(0, &buf[..recv_len], true) {
                    match e {
                        TransportError::InvalidState => {
                            // The connection was closed, so just exit the loop.
                            break;
                        }
                        _ => {
                            // Some other error occurred, so print an error message and continue.
                            println!("Error sending data: {:?}", e);
                        }
                    }
                }
            }

            println!("Read task exiting.");
        });
    }
}
