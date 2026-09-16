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

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct NatInfo {
    pub is_opened: bool,
    pub external_ip: Option<String>,
    pub external_port: Option<u16>,
    pub method: String,
    pub message: String,
}

#[derive(Clone)]
pub struct PeerManager {
    my_identity: NodeIdentity,
    pub my_port: u16,
    peers: Arc<RwLock<HashMap<String, PeerInfo>>>,
    nat_info: Arc<RwLock<NatInfo>>,
}

impl PeerManager {
    pub fn new(my_identity: NodeIdentity, my_port: u16) -> Self {
        let default_nat = NatInfo {
            is_opened: false,
            external_ip: None,
            external_port: None,
            method: "탐색 초기화 중".to_string(),
            message: "NAT 공유기 상태 확인 중...".to_string(),
        };
        PeerManager {
            my_identity,
            my_port,
            peers: Arc::new(RwLock::new(HashMap::new())),
            nat_info: Arc::new(RwLock::new(default_nat)),
        }
    }

    pub fn get_nat_info(&self) -> NatInfo {
        self.nat_info.read().clone()
    }

    pub fn set_nat_info(&self, info: NatInfo) {
        *self.nat_info.write() = info;
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

        if remote_ident.node_id == self.my_identity.node_id {
            return Err("자기 자신 노드입니다".to_string());
        }

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

/// Query STUN server (RFC 5389) for public IP and mapped port
pub fn try_stun() -> Option<(String, u16)> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.set_read_timeout(Some(Duration::from_millis(2000))).ok()?;

    let mut packet = vec![0u8; 20];
    packet[0] = 0x00; packet[1] = 0x01; // Binding Request
    packet[2] = 0x00; packet[3] = 0x00; // Length = 0
    packet[4] = 0x21; packet[5] = 0x12; packet[6] = 0xa4; packet[7] = 0x42; // Magic Cookie
    for i in 8..20 {
        packet[i] = (i * 23) as u8;
    }

    socket.send_to(&packet, "stun.l.google.com:19302").ok()?;
    let mut buf = [0u8; 1024];
    let (len, _) = socket.recv_from(&mut buf).ok()?;
    if len < 20 { return None; }

    let msg_len = ((buf[2] as usize) << 8) | (buf[3] as usize);
    let mut idx = 20;
    while idx + 4 <= 20 + msg_len && idx + 4 <= len {
        let attr_type = ((buf[idx] as u16) << 8) | (buf[idx + 1] as u16);
        let attr_len = ((buf[idx + 2] as usize) << 8) | (buf[idx + 3] as usize);
        idx += 4;
        if attr_type == 0x0020 && idx + attr_len <= len && attr_len >= 8 {
            let xport = ((buf[idx + 2] as u16) << 8) | (buf[idx + 3] as u16);
            let port = xport ^ 0x2112;
            let ip = format!("{}.{}.{}.{}",
                buf[idx + 4] ^ 0x21,
                buf[idx + 5] ^ 0x12,
                buf[idx + 6] ^ 0xa4,
                buf[idx + 7] ^ 0x42
            );
            return Some((ip, port));
        }
        idx += (attr_len + 3) & !3;
    }
    None
}

/// Query UPnP IGD on local router to automatically open public port
pub fn try_upnp(local_ip: &str, port: u16) -> Option<(String, u16)> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.set_read_timeout(Some(Duration::from_millis(1500))).ok()?;
    socket.set_broadcast(true).ok()?;

    let probe = "M-SEARCH * HTTP/1.1\r\n\
                 HOST: 239.255.255.250:1900\r\n\
                 MAN: \"ssdp:discover\"\r\n\
                 MX: 2\r\n\
                 ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\r\n";

    socket.send_to(probe.as_bytes(), "239.255.255.250:1900").ok()?;
    let mut buf = [0u8; 2048];
    let (len, _) = socket.recv_from(&mut buf).ok()?;
    let resp = String::from_utf8_lossy(&buf[..len]);

    let mut location = None;
    for line in resp.lines() {
        let l = line.to_lowercase();
        if l.starts_with("location:") {
            location = Some(line["location:".len()..].trim().to_string());
            break;
        }
    }
    let loc_url = location?;

    let stripped = loc_url.strip_prefix("http://")?;
    let (host_port, xml_path) = match stripped.find('/') {
        Some(i) => (&stripped[..i], &stripped[i..]),
        None => (stripped, "/"),
    };

    let mut stream = TcpStream::connect(host_port).ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(2000))).ok()?;
    let get_req = format!("GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", xml_path, host_port);
    stream.write_all(get_req.as_bytes()).ok()?;
    let mut xml_resp = String::new();
    stream.read_to_string(&mut xml_resp).ok()?;

    let service_marker = "urn:schemas-upnp-org:service:WANIPConnection:1";
    let marker_idx = xml_resp.find(service_marker).or_else(|| xml_resp.find("urn:schemas-upnp-org:service:WANPPPConnection:1"))?;
    let rest = &xml_resp[marker_idx..];
    let cu_start = rest.find("<controlURL>")? + "<controlURL>".len();
    let cu_end = rest[cu_start..].find("</controlURL>")? + cu_start;
    let control_path = rest[cu_start..cu_end].trim();

    // 1. GetExternalIPAddress SOAP
    let soap_get_ip = "<?xml version=\"1.0\"?>\r\n\
        <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\r\n\
        <s:Body><u:GetExternalIPAddress xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:1\"/></s:Body>\r\n\
        </s:Envelope>";

    let mut stream2 = TcpStream::connect(host_port).ok()?;
    stream2.set_read_timeout(Some(Duration::from_millis(2000))).ok()?;
    let post_req = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nSOAPAction: \"urn:schemas-upnp-org:service:WANIPConnection:1#GetExternalIPAddress\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        control_path, host_port, soap_get_ip.len(), soap_get_ip
    );
    stream2.write_all(post_req.as_bytes()).ok()?;
    let mut ip_resp = String::new();
    stream2.read_to_string(&mut ip_resp).ok()?;

    let ip_start = ip_resp.find("<NewExternalIPAddress>")? + "<NewExternalIPAddress>".len();
    let ip_end = ip_resp[ip_start..].find("</NewExternalIPAddress>")? + ip_start;
    let external_ip = ip_resp[ip_start..ip_end].trim().to_string();

    // 2. AddPortMapping SOAP
    let soap_add_port = format!(
        "<?xml version=\"1.0\"?>\r\n\
        <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\r\n\
        <s:Body><u:AddPortMapping xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:1\">\r\n\
          <NewRemoteHost></NewRemoteHost>\r\n\
          <NewExternalPort>{}</NewExternalPort>\r\n\
          <NewProtocol>TCP</NewProtocol>\r\n\
          <NewInternalPort>{}</NewInternalPort>\r\n\
          <NewInternalClient>{}</NewInternalClient>\r\n\
          <NewEnabled>1</NewEnabled>\r\n\
          <NewPortMappingDescription>Aether Node P2P</NewPortMappingDescription>\r\n\
          <NewLeaseDuration>3600</NewLeaseDuration>\r\n\
        </u:AddPortMapping></s:Body>\r\n\
        </s:Envelope>",
        port, port, local_ip
    );

    let mut stream3 = TcpStream::connect(host_port).ok()?;
    stream3.set_read_timeout(Some(Duration::from_millis(2000))).ok()?;
    let post_map = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nSOAPAction: \"urn:schemas-upnp-org:service:WANIPConnection:1#AddPortMapping\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        control_path, host_port, soap_add_port.len(), soap_add_port
    );
    stream3.write_all(post_map.as_bytes()).ok()?;
    let mut map_resp = String::new();
    stream3.read_to_string(&mut map_resp).ok()?;

    if map_resp.contains("AddPortMappingResponse") || map_resp.contains("200 OK") {
        Some((external_ip, port))
    } else {
        None
    }
}

/// Start background thread to probe NAT, run UPnP port forwarding and STUN discovery
pub fn start_nat_traversal(peer_mgr: PeerManager, local_ip: String, port: u16) {
    thread::spawn(move || {
        // 1. Try UPnP IGD
        if let Some((ext_ip, ext_port)) = try_upnp(&local_ip, port) {
            let info = NatInfo {
                is_opened: true,
                external_ip: Some(ext_ip.clone()),
                external_port: Some(ext_port),
                method: "UPnP IGD (MiniUPnPd)".to_string(),
                message: format!("공유기(NAT) 자동 포트 개방 완료 ({}:{})", ext_ip, ext_port),
            };
            peer_mgr.set_nat_info(info);
            println!(" \x1b[1;32m✔ [공유기 NAT 개방]\x1b[0m UPnP IGD 자동 포트포워딩 성공! (공인망: {}:{})", ext_ip, ext_port);
            return;
        }

        // 2. Fallback to STUN
        if let Some((ext_ip, ext_port)) = try_stun() {
            let info = NatInfo {
                is_opened: false,
                external_ip: Some(ext_ip.clone()),
                external_port: Some(ext_port),
                method: "STUN (RFC 5389)".to_string(),
                message: format!("공인 IP 감지됨 ({}:{})", ext_ip, ext_port),
            };
            peer_mgr.set_nat_info(info);
            println!(" \x1b[1;33mℹ [STUN 공인 IP 감지]\x1b[0m {}:{} (수동 포트포워딩 필요)", ext_ip, ext_port);
            return;
        }

        // 3. Local LAN only
        let info = NatInfo {
            is_opened: false,
            external_ip: None,
            external_port: None,
            method: "Local LAN".to_string(),
            message: "로컬 사설망 전용 가동 중".to_string(),
        };
        peer_mgr.set_nat_info(info);
    });
}

/// Primary Bootnode Seed List (Public WAN and LAN fallbacks)
pub const DEFAULT_BOOTNODES: &[&str] = &[
    "14.32.162.195:8080", // Jay's Sovereign Seed Node (Public WAN UPnP)
    "192.168.0.4:8080",   // LAN fallback
];

/// Automatically connect to seed bootnodes upon startup
pub fn start_bootnode_discovery(peer_mgr: PeerManager) {
    thread::spawn(move || {
        // Wait 1.5s for local TCP server to start
        thread::sleep(Duration::from_millis(1500));
        for &seed in DEFAULT_BOOTNODES {
            if peer_mgr.contains_endpoint(seed) {
                continue;
            }
            if let Ok(info) = peer_mgr.connect_peer(seed) {
                println!(" \x1b[1;32m✔ [부트노드 자동 피어링]\x1b[0m 시드 노드({}: {}) 연결 성공!", seed, info.node_id);
                break;
            }
        }
    });
}

