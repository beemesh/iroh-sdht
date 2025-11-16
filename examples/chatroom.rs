use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Parser;
use iroh::endpoint::{Connection, Endpoint, RelayMode};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::EndpointAddr;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use iroh_sdht::{
    derive_node_id, hash_content, Contact, DhtProtocolHandler, DiscoveryNode, IrohNetwork, Key,
    DHT_ALPN,
};

const CHAT_ALPN: &[u8] = b"iroh-chatroom/1";

/// CLI arguments for the chatroom example.
#[derive(Parser, Debug)]
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

struct ChatProtocolHandler;

impl fmt::Debug for ChatProtocolHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatProtocolHandler").finish()
    }
}

impl ProtocolHandler for ChatProtocolHandler {
    fn accept(
        &self,
        connection: Connection,
    ) -> impl std::future::Future<Output = Result<(), AcceptError>> + Send {
        async move {
            handle_chat_connection(connection).await.map_err(|err| {
                AcceptError::from_err(std::io::Error::new(std::io::ErrorKind::Other, err))
            })
        }
    }
}

/// Handle an inbound CHAT_ALPN connection:
/// - Read one framed ChatMessage
/// - Print it
async fn handle_chat_connection(connection: Connection) -> Result<()> {
    // Accept a single bidi stream for the chat message.
    let (mut send, mut recv) = connection.accept_bi().await?;

    // Simple length-prefixed frame:
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;

    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;

    let msg: ChatMessage = serde_json::from_slice(&buf).context("invalid ChatMessage JSON")?;

    println!("[{}] {}: {}", msg.room, msg.author, msg.body,);

    // Optionally ack; here we just send an empty frame as an ACK.
    send.write_all(&0u32.to_be_bytes()).await?;
    send.flush().await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = ChatArgs::parse();

    let endpoint = Endpoint::builder()
        .alpns(vec![DHT_ALPN.to_vec(), CHAT_ALPN.to_vec()])
        .relay_mode(RelayMode::Default)
        .bind()
        .await?;

    // NOTE: iroh 0.95.1 does not expose endpoint::util::enable_local_mdns; skip it.

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

    // DHT is still used for peer discovery, but not for storing chat messages.
    let dht = DiscoveryNode::new(node_id, self_contact.clone(), network, 20, 3);

    let _router = Router::builder(endpoint.clone())
        .accept(DHT_ALPN, DhtProtocolHandler::new(dht.clone()))
        .accept(CHAT_ALPN, ChatProtocolHandler)
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

/// Derive a DHT key for the room to use for peer discovery.
/// This *does not* store anything in the DHT; it only gives us a target
/// for `iterative_find_node`-style queries on the routing table.
fn room_discovery_key(room: &str) -> Key {
    // Reuse the crate’s hash-based key-derivation helper so the format matches the DHT.
    hash_content(room.as_bytes())
}

async fn run_repl(
    endpoint: Endpoint,
    dht: DiscoveryNode<IrohNetwork>,
    peers: Arc<Mutex<Vec<EndpointAddr>>>,
    nickname: String,
    room: String,
    self_contact: Contact,
) -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdin_reader = tokio::io::BufReader::new(stdin).lines();

    while let Some(line) = stdin_reader.next_line().await? {
        let line = line.trim().to_string();

        if line.is_empty() {
            continue;
        }

        if line.starts_with("/add ") {
            let json = line["/add ".len()..].trim();
            match serde_json::from_str::<EndpointAddr>(json) {
                Ok(addr) => {
                    peers.lock().await.push(addr);
                    println!("Added peer.");
                }
                Err(err) => {
                    eprintln!("Invalid EndpointAddr JSON: {err:?}");
                }
            }
            continue;
        }

        if line == "/peers" {
            let list = peers.lock().await;
            println!("Known peers:");
            for (i, p) in list.iter().enumerate() {
                println!("  [{i}] {p:?}");
            }
            continue;
        }

        if line == "/quit" {
            println!("Exiting.");
            break;
        }

        // Regular chat message: build ChatMessage.
        let msg = ChatMessage {
            room: room.clone(),
            author: nickname.clone(),
            body: line.clone(),
            timestamp: current_unix_time(),
        };

        // Serialize it once.
        let msg_bytes = serde_json::to_vec(&msg)?;

        // 1. Local peers from CLI + /add.
        let static_peers = peers.lock().await.clone();

        // 2. DHT-based peers: ask the DHT for nodes close to a room-discovery key
        //    via a regular iterative_find_node lookup.
        let discovery_key = room_discovery_key(&room);
        let dynamic_peers = dht
            .iterative_find_node(discovery_key)
            .await
            .unwrap_or_default();

        // Merge peers:
        // - From static_peers: EndpointAddr
        // - From dynamic_peers: Contact { addr: EndpointAddr JSON }
        let mut all_peer_addrs: Vec<EndpointAddr> = static_peers;

        for contact in dynamic_peers {
            if let Ok(addr) = serde_json::from_str::<EndpointAddr>(&contact.addr) {
                // Avoid duplicates
                if !all_peer_addrs.iter().any(|p| p == &addr) {
                    all_peer_addrs.push(addr);
                }
            }
        }

        if all_peer_addrs.is_empty() {
            println!("No peers to send to (yet).");
            continue;
        }

        // Send the message to every peer directly; no DHT put/get.
        for peer in all_peer_addrs {
            if let Err(err) = send_chat_message(&endpoint, &peer, &msg_bytes, &self_contact).await {
                eprintln!("Failed to send to peer {peer:?}: {err:?}");
            }
        }
    }

    Ok(())
}

/// Send a ChatMessage (already serialized as `msg_bytes`) directly to a peer.
/// This uses the CHAT_ALPN protocol and a simple length-prefixed frame.
async fn send_chat_message(
    endpoint: &Endpoint,
    peer: &EndpointAddr,
    msg_bytes: &[u8],
    _self_contact: &Contact,
) -> Result<()> {
    let conn = endpoint
        .connect(peer.clone(), CHAT_ALPN)
        .await
        .context("failed to connect to peer for chat")?;

    let (mut send, mut recv) = conn.open_bi().await?;

    let len = msg_bytes.len() as u32;
    send.write_all(&len.to_be_bytes()).await?;
    send.write_all(msg_bytes).await?;
    send.flush().await?;

    // Optional: read ACK frame
    let mut ack_len_buf = [0u8; 4];
    if recv.read_exact(&mut ack_len_buf).await.is_ok() {
        // ignore ack body
    }

    Ok(())
}

fn current_unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
