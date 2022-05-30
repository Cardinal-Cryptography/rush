use crate::{Block, Data};
use aleph_bft::Recipient;
use aleph_bft_mock::{Hasher64, PartialMultisignature, Signature};
use codec::{Decode, Encode};
use futures::{
    channel::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    FutureExt, StreamExt,
};
use futures_timer::Delay;
use log::{debug, error, warn};
use std::{
    collections::HashMap,
    error::Error,
    io::Write,
    net::{SocketAddr, SocketAddrV4, TcpStream},
    str::FromStr,
};
use tokio::{io::AsyncReadExt, net::TcpListener};

pub type NetworkData = aleph_bft::NetworkData<Hasher64, Data, Signature, PartialMultisignature>;

#[derive(Clone, Debug, Decode, Encode, PartialEq, Eq)]
pub struct Address {
    octets: [u8; 4],
    port: u16,
}

impl From<SocketAddrV4> for Address {
    fn from(addr: SocketAddrV4) -> Self {
        Self {
            octets: addr.ip().octets(),
            port: addr.port(),
        }
    }
}

impl From<SocketAddr> for Address {
    fn from(addr: SocketAddr) -> Self {
        match addr {
            SocketAddr::V4(addr) => addr.into(),
            SocketAddr::V6(_) => panic!(),
        }
    }
}

impl FromStr for Address {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(s.parse::<SocketAddr>().unwrap().into())
    }
}

impl Address {
    pub async fn new_bind(ip_addr: String) -> (TcpListener, Self) {
        let listener = TcpListener::bind(ip_addr.parse::<SocketAddr>().unwrap())
            .await
            .unwrap();
        let ip_addr = listener.local_addr().unwrap().to_string();
        (listener, Self::from_str(&ip_addr).unwrap())
    }

    pub fn connect(&self) -> std::io::Result<TcpStream> {
        TcpStream::connect(SocketAddr::from((self.octets, self.port)))
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Decode, Encode, Debug)]
enum Message {
    DNSHello(u32, Address),
    DNSRequest(u32, Address),
    DNSResponse(Vec<Option<Address>>),
    Consensus(NetworkData),
    Block(Block),
}

pub struct Network {
    msg_to_manager_tx: mpsc::UnboundedSender<(NetworkData, Recipient)>,
    msg_from_manager_rx: mpsc::UnboundedReceiver<NetworkData>,
}

#[async_trait::async_trait]
impl aleph_bft::Network<NetworkData> for Network {
    fn send(&self, data: NetworkData, recipient: Recipient) {
        if let Err(e) = self.msg_to_manager_tx.unbounded_send((data, recipient)) {
            warn!(target: "Blockchain-network", "Failed network send: {:?}", e);
        }
    }
    async fn next_event(&mut self) -> Option<NetworkData> {
        self.msg_from_manager_rx.next().await
    }
}

pub struct NetworkManager {
    id: usize,
    address: Address,
    addresses: Vec<Option<Address>>,
    bootnodes: HashMap<u32, Address>,
    listener: TcpListener,
    consensus_tx: UnboundedSender<NetworkData>,
    consensus_rx: UnboundedReceiver<(NetworkData, Recipient)>,
    block_tx: UnboundedSender<Block>,
    block_rx: UnboundedReceiver<Block>,
}

impl NetworkManager {
    pub async fn new(
        id: usize,
        ip_addr: String,
        n_nodes: usize,
        bootnodes: HashMap<u32, Address>,
    ) -> Result<
        (
            Self,
            Network,
            UnboundedSender<Block>,
            UnboundedReceiver<Block>,
            UnboundedSender<NetworkData>,
            UnboundedReceiver<NetworkData>,
        ),
        Box<dyn Error>,
    > {
        let mut addresses = vec![None; n_nodes];
        for (id, addr) in &bootnodes {
            addresses[*id as usize] = Some(addr.clone());
        }
        let (listener, address) = Address::new_bind(ip_addr).await;
        addresses[id] = Some(address.clone());

        let (msg_to_manager_tx, msg_to_manager_rx) = mpsc::unbounded();
        let (msg_for_store, msg_from_manager) = mpsc::unbounded();
        let (msg_for_network, msg_from_store) = mpsc::unbounded();
        let (block_to_data_io_tx, block_to_data_io_rx) = mpsc::unbounded();
        let (block_from_data_io_tx, block_from_data_io_rx) = mpsc::unbounded();

        let network = Network {
            msg_to_manager_tx,
            msg_from_manager_rx: msg_from_store,
        };

        let network_manager = NetworkManager {
            id,
            address,
            addresses,
            bootnodes,
            listener,
            consensus_tx: msg_for_store,
            consensus_rx: msg_to_manager_rx,
            block_tx: block_to_data_io_tx,
            block_rx: block_from_data_io_rx,
        };

        Ok((
            network_manager,
            network,
            block_from_data_io_tx,
            block_to_data_io_rx,
            msg_for_network,
            msg_from_manager,
        ))
    }

    fn recipient_to_addresses(&self, recipient: Recipient) -> HashMap<usize, Address> {
        let mut addr: HashMap<usize, Address> = HashMap::new();
        match recipient {
            Recipient::Node(n) => {
                let n: usize = n.into();
                assert!(n < self.addresses.len());
                if n != self.id {
                    if let Some(a) = &self.addresses[n] {
                        addr.insert(n, a.clone());
                    }
                }
            }
            Recipient::Everyone => {
                for n in 0..self.addresses.len() {
                    if n != self.id {
                        if let Some(a) = &self.addresses[n] {
                            addr.insert(n, a.clone());
                        }
                    };
                }
            }
        }
        addr
    }

    fn reset_dns(&mut self, n: usize) {
        if !self.bootnodes.contains_key(&(n as u32)) {
            error!("Reseting address of node {}", n);
            self.addresses[n] = None;
        }
    }

    fn send(&mut self, message: Message, recipient: Recipient) {
        let addr = self.recipient_to_addresses(recipient);
        for (n, addr) in addr.into_iter() {
            match self.try_send(message.clone(), &addr) {
                Ok(_) => (),
                Err(_) => self.reset_dns(n),
            };
        }
    }

    fn try_send(&self, message: Message, address: &Address) -> std::io::Result<()> {
        debug!("Trying to send message {:?} to {:?}", message, address);
        address.connect()?.write_all(&message.encode())
    }

    fn dns_response(&mut self, id: usize, address: Address) {
        self.addresses[id] = Some(address.clone());
        self.try_send(Message::DNSResponse(self.addresses.clone()), &address)
            .unwrap_or(());
    }

    pub async fn run(&mut self, mut exit: oneshot::Receiver<()>) {
        let dns_ticker_delay = std::time::Duration::from_millis(1000);
        let mut dns_ticker = Delay::new(dns_ticker_delay).fuse();
        let dns_hello_ticker_delay = std::time::Duration::from_millis(5000);
        let mut dns_hello_ticker = Delay::new(dns_hello_ticker_delay).fuse();

        loop {
            let mut buffer = Vec::new();
            tokio::select! {

                event = self.listener.accept() => match event {
                    Ok((mut socket, _addr)) => {
                        match socket.read_to_end(&mut buffer).await {
                            Ok(_) => {
                                let message = Message::decode(&mut &buffer[..]);
                                debug!("Received message: {:?}", message);
                                match message {
                                    Ok(Message::Consensus(data)) => self.consensus_tx.unbounded_send(data).expect("Network must listen"),
                                    Ok(Message::Block(block)) => {
                                        debug!(target: "Blockchain-network", "Received block num {:?}", block.num);
                                        self.block_tx
                                            .unbounded_send(block)
                                            .expect("Blockchain process must listen");
                                    },
                                    Ok(Message::DNSHello(id, address)) => {
                                        self.addresses[id as usize] = Some(address);
                                    },
                                    Ok(Message::DNSRequest(id, address)) => self.dns_response(id as usize, address),
                                    Ok(Message::DNSResponse(addresses)) => for (id, addr) in addresses.iter().enumerate() {
                                        if let Some(addr) = addr {
                                            self.addresses[id as usize] = Some(addr.clone());
                                        };
                                    },
                                    Err(_) => (),
                                };
                            },
                            Err(_) => {
                                error!("Could not decode incoming data");
                            },
                        }
                    },
                    Err(e) => {
                        error!("Couldn't accept connection: {:?}", e);
                    },
                },

                _ = &mut dns_ticker => {
                    if self.addresses.iter().any(|a| a.is_none()) {
                        self.send(Message::DNSRequest(self.id as u32, self.address.clone()), Recipient::Everyone);
                        debug!("Requesting IP addresses");
                    }
                    dns_ticker = Delay::new(dns_ticker_delay).fuse();
                },

                _ = &mut dns_hello_ticker => {
                    self.send(Message::DNSHello(self.id as u32, self.address.clone()), Recipient::Everyone);
                    debug!("Sending Hello!");
                    dns_hello_ticker = Delay::new(dns_hello_ticker_delay).fuse();
                },

                maybe_msg = self.consensus_rx.next() => {
                    if let Some((consensus_msg, recipient)) = maybe_msg {
                        self.send(Message::Consensus(consensus_msg), recipient);
                    }
                }

                maybe_block = self.block_rx.next() => {
                    if let Some(block) = maybe_block {
                        debug!(target: "Blockchain-network", "Sending block message num {:?}.", block.num);
                        self.send(Message::Block(block), Recipient::Everyone);
                    }
                }

               _ = &mut exit  => break,
            }
        }
    }
}
