use crate::types::{Address, GossipMessage, NodeIdentity, PeerInfo};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BEACON_PORT: u16 = 8085;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HandshakeRequest {
    pub identity: NodeIdentity,
    pub listen_port: u16,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BeaconPayload {
    pub magic: String,
    pub node_id: String,
    pub address: Address,
    pub port: u16,
}

#[derive(Clone)]
pub struct PeerManager {
    my_identity: NodeIdentity,
    pub my_port: u16,
    peers: Arc<RwLock<HashMap<String, PeerInfo>>>,
}

impl PeerManager {
    pub fn new(my_identity: NodeIdentity, my_port: u16) -> Self {
        PeerManager {
            my_identity,
            my_port,
            peers: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn get_my_identity(&self) -> NodeIdentity {
        self.my_identity.clone()
    }

    pub fn add_or_update(&self, mut peer: PeerInfo) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        peer.last_seen_ms = now;
        self.peers.write().insert(peer.endpoint.clone(), peer);
    }

    pub fn remove(&self, endpoint: &str) {
        self.peers.write().remove(endpoint);
    }

    pub fn list(&self) -> Vec<PeerInfo> {
        self.peers.read().values().cloned().collect()
    }

    pub fn count(&self) -> usize {
        self.peers.read().len()
    }

    pub fn contains_endpoint(&self, endpoint: &str) -> bool {
        self.peers.read().contains_key(endpoint)
    }

    pub fn connect_peer(&self, remote_endpoint: &str) -> Result<PeerInfo, String> {
        let mut trimmed = remote_endpoint.trim();
        // Support standard aether://<node_id>@<host>:<port> or aether://<host>:<port>
        if let Some(rest) = trimmed.strip_prefix("aether://") {
            if let Some(idx) = rest.rfind('@') {
                trimmed = &rest[idx + 1..];
            } else {
                trimmed = rest;
            }
        }
        let trimmed = trimmed.trim_start_matches("http://");
        if trimmed.is_empty() {
            return Err("빈 피어 주소입니다".to_string());
        }

        let start = Instant::now();
        // Handshake: send ping via HTTP POST /api/p2p/handshake
        let hs_req = HandshakeRequest {
            identity: self.my_identity.clone(),
            listen_port: self.my_port,
        };
        let req_body = serde_json::to_string(&hs_req).map_err(|e| e.to_string())?;
        let res = http_post(trimmed, "/api/p2p/handshake", &req_body, Duration::from_secs(3))?;
        let latency = start.elapsed().as_millis() as u64;

        let remote_ident: NodeIdentity =
            serde_json::from_str(&res).map_err(|e| format!("응답 해석 실패: {}", e))?;

        let peer_info = PeerInfo {
            node_id: remote_ident.node_id,
            address: remote_ident.address,
            endpoint: trimmed.to_string(),
            last_seen_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            latency_ms: latency.max(1),
            round: 0,
        };

        self.add_or_update(peer_info.clone());
        Ok(peer_info)
    }

    pub fn broadcast_gossip(&self, msg: &GossipMessage) {
        let peers = self.list();
        let payload = match serde_json::to_string(msg) {
            Ok(p) => p,
            Err(_) => return,
        };

        for peer in peers {
            let endpoint = peer.endpoint.clone();
            let body = payload.clone();
            thread::spawn(move || {
                let _ = http_post(&endpoint, "/api/p2p/gossip", &body, Duration::from_secs(2));
            });
        }
    }
}

/// Helper function to detect local LAN IP (e.g. 192.168.x.x)
pub fn get_local_ip() -> String {
    // Open a dummy UDP socket to 8.8.8.8 to query OS routing table
    if let Ok(socket) = UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("8.8.8.8:80").is_ok() {
            if let Ok(local_addr) = socket.local_addr() {
                return local_addr.ip().to_string();
            }
        }
    }
    "127.0.0.1".to_string()
}

/// Start LAN UDP Auto-Discovery beacon broadcaster and listener
pub fn start_lan_auto_discovery(peer_mgr: PeerManager, my_port: u16) {
    let my_ident = peer_mgr.get_my_identity();
    let my_id = my_ident.node_id.clone();
    let my_addr = my_ident.address;

    // 1. UDP Listener Thread
    let peer_mgr_clone = peer_mgr.clone();
    let my_id_clone = my_id.clone();
    thread::spawn(move || {
        let socket = match UdpSocket::bind(format!("0.0.0.0:{}", BEACON_PORT)) {
            Ok(s) => s,
            Err(_) => {
                // If 8085 is bound by another instance on the same machine, bind random port and just listen/broadcast
                match UdpSocket::bind("0.0.0.0:0") {
                    Ok(s) => s,
                    Err(_) => return,
                }
            }
        };
        let _ = socket.set_read_timeout(Some(Duration::from_secs(2)));

        let mut buf = [0u8; 1024];
        loop {
            if let Ok((len, src)) = socket.recv_from(&mut buf) {
                if let Ok(beacon) = serde_json::from_slice::<BeaconPayload>(&buf[..len]) {
                    if beacon.magic == "AETHER_BEACON" && beacon.node_id != my_id_clone {
                        let peer_endpoint = format!("{}:{}", src.ip(), beacon.port);
                        if !peer_mgr_clone.contains_endpoint(&peer_endpoint) {
                            let _ = peer_mgr_clone.connect_peer(&peer_endpoint);
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(500));
        }
    });

    // 2. UDP Broadcaster Thread
    thread::spawn(move || {
        let socket = match UdpSocket::bind("0.0.0.0:0") {
            Ok(s) => s,
            Err(_) => return,
        };
        let _ = socket.set_broadcast(true);

        let beacon = BeaconPayload {
            magic: "AETHER_BEACON".to_string(),
            node_id: my_id,
            address: my_addr,
            port: my_port,
        };
        let data = serde_json::to_vec(&beacon).unwrap_or_default();

        loop {
            let _ = socket.send_to(&data, format!("255.255.255.255:{}", BEACON_PORT));
            thread::sleep(Duration::from_secs(3));
        }
    });
}

/// Simple lightweight HTTP POST client with timeout
pub fn http_post(endpoint: &str, path: &str, body: &str, timeout: Duration) -> Result<String, String> {
    let stream_res = TcpStream::connect(endpoint);
    let mut stream = match stream_res {
        Ok(s) => s,
        Err(e) => return Err(format!("피어 연결 실패 ({}): {}", endpoint, e)),
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));

    let request = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        path,
        endpoint,
        body.len(),
        body
    );

    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("데이터 송신 실패: {}", e))?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| format!("응답 수신 실패: {}", e))?;

    // Extract HTTP body
    if let Some(idx) = response.find("\r\n\r\n") {
        Ok(response[idx + 4..].to_string())
    } else {
        Ok(response)
    }
}

/// Simple lightweight HTTP GET client with timeout
pub fn http_get(endpoint: &str, path: &str, timeout: Duration) -> Result<String, String> {
    let stream_res = TcpStream::connect(endpoint);
    let mut stream = match stream_res {
        Ok(s) => s,
        Err(e) => return Err(format!("피어 연결 실패 ({}): {}", endpoint, e)),
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));

    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        path, endpoint
    );

    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("요청 송신 실패: {}", e))?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| format!("응답 수신 실패: {}", e))?;

    if let Some(idx) = response.find("\r\n\r\n") {
        Ok(response[idx + 4..].to_string())
    } else {
        Ok(response)
    }
}
