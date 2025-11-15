use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use iroh::discovery::mdns::MdnsDiscovery;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, RelayMode};
use tokio::io::{self, AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};

use iroh_sdht::framing::{read_frame, write_frame};
use iroh_sdht::{
    derive_node_id, Contact, DhtNetwork, DhtNode, DhtProtocolHandler, IrohNetwork, Key, DHT_ALPN,
};

const CHAT_ALPN: &[u8] = b"iroh-sdht/chat/0";

#[derive(Debug, Parser)]
#[command(about = "Simple console chatroom built on iroh-sdht")]
struct ChatArgs {
    /// Nickname announced in the chatroom
    #[arg(long, default_value = "anon")]
    name: String,
    /// Room label that is embedded in each message
    #[arg(long, default_value = "lobby")]
    room: String,
    /// JSON-encoded EndpointAddr peers to notify directly
    #[arg(long = "peer")]
    peers: Vec<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ChatMessage {
    room: String,
    author: String,
    body: String,
    timestamp: u64,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ChatPointer {
    key_hex: String,
    from: Contact,
}

#[derive(Clone)]
struct ChatProtocolHandler<N: DhtNetwork> {
    node: Arc<DhtNode<N>>,
}

impl<N: DhtNetwork> ChatProtocolHandler<N> {
    fn new(node: Arc<DhtNode<N>>) -> Self {
        Self { node }
    }
}

impl<N: DhtNetwork> fmt::Debug for ChatProtocolHandler<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatProtocolHandler").finish()
    }
}

impl<N: DhtNetwork> ProtocolHandler for ChatProtocolHandler<N> {
    fn accept(
        &self,
        connection: Connection,
    ) -> impl std::future::Future<Output = Result<(), AcceptError>> + Send {
        let node = self.node.clone();
        async move {
            handle_chat_connection(node, connection)
                .await
                .map_err(|err| {
                    AcceptError::from_err(std::io::Error::new(std::io::ErrorKind::Other, err))
                })
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = ChatArgs::parse();

    let endpoint = Endpoint::builder()
        .alpns(vec![DHT_ALPN.to_vec(), CHAT_ALPN.to_vec()])
        .relay_mode(RelayMode::Default)
        .bind()
        .await?;

    if let Err(err) = enable_local_mdns(&endpoint) {
        eprintln!("mDNS discovery disabled: {err:?}");
    }

    let node_id = derive_node_id(endpoint.id().as_bytes());
    let endpoint_addr: EndpointAddr = endpoint.addr();
    let addr_json = serde_json::to_string(&endpoint_addr)?;
    println!("Chatroom node ready");
    println!("  Nickname        : {}", args.name);
    println!("  Room            : {}", args.room);
    println!("  NodeId (hex)    : {}", hex::encode(node_id));
    println!("  Endpoint addr   : {addr_json}");
    println!("Share the endpoint address with peers so they can /add it.");

    let self_contact = Contact {
        id: node_id,
        addr: addr_json.clone(),
    };

    let network = IrohNetwork {
        endpoint: endpoint.clone(),
        self_contact: self_contact.clone(),
    };

    let dht = Arc::new(DhtNode::new(node_id, self_contact.clone(), network, 20, 3));

    let _router = Router::builder(endpoint.clone())
        .accept(DHT_ALPN, DhtProtocolHandler::new(dht.clone()))
        .accept(CHAT_ALPN, ChatProtocolHandler::new(dht.clone()))
        .spawn();

    let peer_addrs = parse_initial_peers(args.peers)?;
    let peers = Arc::new(Mutex::new(peer_addrs));

    println!("\nCommands:");
    println!("  /add <addr-json>  - add a peer endpoint address");
    println!("  /peers            - list known peers");
    println!("  /quit             - exit");
    println!("Type anything else to send it to the room.\n");

    run_repl(
        endpoint.clone(),
        dht.clone(),
        peers.clone(),
        args.name.clone(),
        args.room.clone(),
        self_contact,
    )
    .await
}

fn parse_initial_peers(raw: Vec<String>) -> Result<Vec<EndpointAddr>> {
    raw.into_iter()
        .map(|entry| serde_json::from_str(&entry).context("invalid EndpointAddr JSON"))
        .collect()
}

async fn run_repl(
    endpoint: Endpoint,
    dht: Arc<DhtNode<IrohNetwork>>,
    peers: Arc<Mutex<Vec<EndpointAddr>>>,
    nickname: String,
    room: String,
    self_contact: Contact,
) -> Result<()> {
    let stdin = BufReader::new(io::stdin());
    let mut lines = stdin.lines();

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed == "/quit" {
            println!("Bye!\n");
            break;
        } else if let Some(addr) = trimmed.strip_prefix("/add ") {
            match serde_json::from_str::<EndpointAddr>(addr.trim()) {
                Ok(parsed) => {
                    let mut guard = peers.lock().await;
                    if !guard.contains(&parsed) {
                        guard.push(parsed.clone());
                        println!("Added peer: {addr}");
                    } else {
                        println!("Peer already added");
                    }
                }
                Err(err) => {
                    eprintln!("Failed to parse endpoint address: {err:?}");
                }
            }
            continue;
        } else if trimmed == "/peers" {
            let guard = peers.lock().await;
            if guard.is_empty() {
                println!("(no peers configured)");
            } else {
                for (idx, peer) in guard.iter().enumerate() {
                    match serde_json::to_string(peer) {
                        Ok(json) => println!("[{idx}] {json}"),
                        Err(err) => println!("[{idx}] <error serializing>: {err:?}"),
                    }
                }
            }
            continue;
        }

        if let Err(err) = publish_message(
            endpoint.clone(),
            dht.clone(),
            peers.clone(),
            nickname.clone(),
            room.clone(),
            trimmed.to_owned(),
            self_contact.clone(),
        )
        .await
        {
            eprintln!("Failed to publish message: {err:?}");
        }
    }

    Ok(())
}

async fn publish_message(
    endpoint: Endpoint,
    dht: Arc<DhtNode<IrohNetwork>>,
    peers: Arc<Mutex<Vec<EndpointAddr>>>,
    nickname: String,
    room: String,
    body: String,
    self_contact: Contact,
) -> Result<()> {
    let msg = ChatMessage {
        room: room.clone(),
        author: nickname.clone(),
        body: body.clone(),
        timestamp: current_timestamp(),
    };
    let payload = serde_json::to_vec(&msg)?;
    let key = dht.put(payload.clone()).await?;
    println!("[you @ {room}] {body}");

    let pointer = ChatPointer {
        key_hex: hex::encode(key),
        from: self_contact,
    };

    broadcast_pointer(endpoint, peers, pointer).await;

    Ok(())
}

async fn broadcast_pointer(
    endpoint: Endpoint,
    peers: Arc<Mutex<Vec<EndpointAddr>>>,
    pointer: ChatPointer,
) {
    let json = match serde_json::to_vec(&pointer) {
        Ok(bytes) => bytes,
        Err(err) => {
            eprintln!("Failed to encode pointer: {err:?}");
            return;
        }
    };

    let guard = peers.lock().await;
    for peer in guard.iter().cloned() {
        let endpoint = endpoint.clone();
        let json = json.clone();
        tokio::spawn(async move {
            if let Err(err) = send_pointer(endpoint, peer, json).await {
                eprintln!("Failed to send pointer: {err:?}");
            }
        });
    }
}

async fn send_pointer(endpoint: Endpoint, peer: EndpointAddr, payload: Vec<u8>) -> Result<()> {
    let conn = endpoint.connect(peer, CHAT_ALPN).await?;
    let (mut send, _recv) = conn.open_bi().await?;
    write_frame(&mut send, &payload).await?;
    send.finish()?;
    Ok(())
}

async fn handle_chat_connection<N: DhtNetwork>(
    node: Arc<DhtNode<N>>,
    conn: Connection,
) -> Result<()> {
    let (mut send, mut recv): (SendStream, RecvStream) = conn.accept_bi().await?;
    if let Some(bytes) = read_frame(&mut recv).await? {
        let pointer: ChatPointer = serde_json::from_slice(&bytes)?;
        process_pointer(node, pointer).await;
    }
    send.finish()?;
    Ok(())
}

async fn process_pointer<N: DhtNetwork>(node: Arc<DhtNode<N>>, pointer: ChatPointer) {
    node.observe_contact(pointer.from.clone()).await;
    match decode_hex_key(&pointer.key_hex) {
        Ok(key) => {
            let node_clone = node.clone();
            tokio::spawn(async move {
                if let Err(err) = fetch_and_print_message(node_clone, key).await {
                    eprintln!("Failed to fetch message: {err:?}");
                }
            });
        }
        Err(err) => {
            eprintln!("Invalid key from peer: {err:?}");
        }
    }
}

fn decode_hex_key(hex_str: &str) -> Result<Key> {
    let raw = hex::decode(hex_str)?;
    if raw.len() != 32 {
        return Err(anyhow!("expected 32-byte key"));
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&raw);
    Ok(key)
}

async fn fetch_and_print_message<N: DhtNetwork>(node: Arc<DhtNode<N>>, key: Key) -> Result<()> {
    for attempt in 0..3 {
        match node.get(key).await? {
            Some(bytes) => {
                if let Ok(msg) = serde_json::from_slice::<ChatMessage>(&bytes) {
                    println!("[{}] {}: {}", msg.room, msg.author, msg.body);
                } else {
                    println!("<received {key_hex}>", key_hex = hex::encode(key));
                }
                return Ok(());
            }
            None => {
                let delay = Duration::from_secs(1 << attempt);
                sleep(delay).await;
            }
        }
    }
    Err(anyhow!(
        "message {key_hex} not found",
        key_hex = hex::encode(key)
    ))
}

fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn enable_local_mdns(endpoint: &Endpoint) -> anyhow::Result<()> {
    let mdns = MdnsDiscovery::builder()
        .service_name("iroh-sdht-chat")
        .build(endpoint.id())
        .map_err(|err| anyhow::anyhow!("mDNS discovery failed: {err}"))?;
    endpoint.discovery().add(mdns);
    Ok(())
}
