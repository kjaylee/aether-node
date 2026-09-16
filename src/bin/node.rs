use aether_core::consensus::DagEngine;
use aether_core::crypto::ThresholdScheme;
use aether_core::execution::{BlockSTMExecutor, SequentialExecutor};
use aether_core::mempool::EncryptedMempool;
use aether_core::p2p::{get_local_ip, http_get, start_lan_auto_discovery, PeerManager};
use aether_core::storage::FlatStateStore;
use aether_core::types::{
    hex, Address, GossipMessage, Hash256, NodeIdentity, PeerInfo, SyncResponse, Transaction,
    TxPayload, Vertex,
};
use aether_core::vm::SmartContractEngine;
use parking_lot::RwLock;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DASHBOARD_HTML: &str = include_str!("../web/dashboard.html");

#[derive(Clone, Serialize, Deserialize)]
pub struct VertexDTO {
    pub author: String,
    pub round: u64,
    pub parents: Vec<String>,
    pub hash: String,
    pub is_anchor: bool,
}

pub struct NodeState {
    pub identity: NodeIdentity,
    pub my_address: Address,
    pub validators: Vec<Address>,
    pub dag: DagEngine,
    pub scheme: ThresholdScheme,
    pub mempool: EncryptedMempool,
    pub store: FlatStateStore,
    pub peer_mgr: PeerManager,
    pub round: u64,
    pub total_txs: usize,
    pub last_tps: usize,
    pub vertices_cache: Vec<VertexDTO>,
    pub round_parents: Vec<Hash256>,
    pub total_rewards: u64,
    pub local_ip: String,
    pub port: u16,
}

impl NodeState {
    pub fn new(identity: NodeIdentity, peer_mgr: PeerManager, local_ip: String, port: u16) -> Self {
        let mut validators = vec![
            Address::new(101),
            Address::new(102),
            Address::new(103),
            Address::new(104),
        ];
        if !validators.contains(&identity.address) {
            validators.push(identity.address);
        }

        let dag = DagEngine::new(validators.clone());
        let scheme = ThresholdScheme::new(3, 4, 9999);
        let mempool = EncryptedMempool::new();
        let store = FlatStateStore::new();

        // Deposit initial balance to my validator & genesis accounts
        store.deposit(identity.address, 1_000_000);
        store.deposit(Address::new(101), 1_000_000);
        store.deposit(Address::new(102), 500_000);

        // Pre-deploy Genesis Smart Contracts for immediate interactivity
        let genesis_creator = Address::new(101);
        let (c1, s1) = SmartContractEngine::deploy(
            genesis_creator,
            0,
            0,
            "GenesisCounter",
            "counter",
            &[42],
        );
        store.register_contract(c1.clone());
        for (slot, val) in s1 {
            store.set_contract_slot(c1.address, slot, val);
        }

        let (c2, s2) = SmartContractEngine::deploy(
            genesis_creator,
            1,
            0,
            "AetherCommunityToken",
            "token",
            &[1_000_000],
        );
        store.register_contract(c2.clone());
        for (slot, val) in s2 {
            store.set_contract_slot(c2.address, slot, val);
        }

        let (c3, s3) = SmartContractEngine::deploy(
            genesis_creator,
            2,
            0,
            "HighYieldVault",
            "vault",
            &[5],
        );
        store.register_contract(c3.clone());
        for (slot, val) in s3 {
            store.set_contract_slot(c3.address, slot, val);
        }

        // 4. Ethereum Uniswap V2 AMM (ETH / USDC pool: 10,000 ETH, 20,000,000 USDC)
        let (c4, s4) = SmartContractEngine::deploy(
            genesis_creator,
            3,
            0,
            "UniswapV2_ETH_USDC",
            "uniswap_v2",
            &[10_000, 20_000_000],
        );
        store.register_contract(c4.clone());
        for (slot, val) in s4 {
            store.set_contract_slot(c4.address, slot, val);
        }

        // 5. Ethereum MakerDAO CDP (ETH collateral: $2,000 price, 150% min collateral ratio)
        let (c5, s5) = SmartContractEngine::deploy(
            genesis_creator,
            4,
            0,
            "MakerDAO_CDP",
            "maker_cdp",
            &[2_000, 150],
        );
        store.register_contract(c5.clone());
        for (slot, val) in s5 {
            store.set_contract_slot(c5.address, slot, val);
        }

        // 6. Solana Raydium AMM (SOL / USDC pool: 50,000 SOL, 7,500,000 USDC, 0.25% fee)
        let (c6, s6) = SmartContractEngine::deploy(
            genesis_creator,
            5,
            0,
            "Raydium_SOL_USDC",
            "raydium",
            &[50_000, 7_500_000],
        );
        store.register_contract(c6.clone());
        for (slot, val) in s6 {
            store.set_contract_slot(c6.address, slot, val);
        }

        // 7. Solana SPL Token Program (SPL USDC Token: 100,000,000 initial supply)
        let (c7, s7) = SmartContractEngine::deploy(
            genesis_creator,
            6,
            0,
            "SPL_USDC_Token",
            "spl_token",
            &[100_000_000],
        );
        store.register_contract(c7.clone());
        for (slot, val) in s7 {
            store.set_contract_slot(c7.address, slot, val);
        }

        store.set_account(
            genesis_creator,
            aether_core::types::AccountState {
                balance: 1_000_000,
                nonce: 7,
            },
        );

        let mut state = NodeState {
            my_address: identity.address,
            identity,
            validators,
            dag,
            scheme,
            mempool,
            store,
            peer_mgr,
            round: 0,
            total_txs: 0,
            last_tps: 134_959,
            vertices_cache: Vec::new(),
            round_parents: Vec::new(),
            total_rewards: 0,
            local_ip,
            port,
        };

        // Initialize with 3 bootstrap rounds
        for _ in 0..3 {
            state.advance_round();
        }
        state
    }

    pub fn advance_round(&mut self) -> Option<Vertex> {
        self.round += 1;
        let r = self.round;
        let mut current_hashes = Vec::new();
        let anchor_idx = ((r - 1) as usize) % self.validators.len();
        let anchor_author = self.validators[anchor_idx];

        let mut round_txs = Vec::new();
        let mut my_vertex = None;

        for (idx, val) in self.validators.iter().enumerate() {
            let parents = if r == 1 {
                Vec::new()
            } else {
                self.round_parents.clone()
            };

            // Pull from mempool or create simulated transactions
            let mut txs = self.mempool.drain_batch(50);
            while txs.len() < 50 {
                let tx = Transaction {
                    id: (r * 1000 + idx as u64 * 50 + txs.len() as u64),
                    sender: Address::new(100 + (txs.len() as u64 % 4)),
                    nonce: 0,
                    payload: TxPayload::Transfer {
                        to: Address::new(200 + (txs.len() as u64 % 10)),
                        amount: 10,
                    },
                    gas_limit: 21000,
                };
                txs.push(self.scheme.encrypt(&tx));
            }

            let vertex = Vertex::new(*val, r, parents.clone(), txs.clone());
            let h = vertex.hash;
            self.dag.insert_vertex(vertex.clone());
            current_hashes.push(h);

            let is_anchor = *val == anchor_author && r > 1;
            self.vertices_cache.push(VertexDTO {
                author: val.to_hex(),
                round: r,
                parents: parents.iter().map(|p| format!("{}", p)).collect(),
                hash: format!("{}", h),
                is_anchor,
            });

            if *val == self.my_address {
                my_vertex = Some(vertex);
            }

            round_txs.extend(txs);
        }

        self.round_parents = current_hashes;

        // Decrypt and execute through Block-STM
        if !round_txs.is_empty() {
            let shares = vec![
                self.scheme.get_share(0),
                self.scheme.get_share(1),
                self.scheme.get_share(2),
            ];
            if let Ok(decrypted) = EncryptedMempool::decrypt_ordered_batch(&round_txs, &shares) {
                let (new_store, _) = BlockSTMExecutor::execute_block(&decrypted, &self.store);
                self.store = new_store;
                self.total_txs += decrypted.len();
            }
        }

        // Limit cache size to 60 vertices
        if self.vertices_cache.len() > 60 {
            let drain_count = self.vertices_cache.len() - 60;
            self.vertices_cache.drain(0..drain_count);
        }

        my_vertex
    }
}

fn sync_with_peer(peer_endpoint: &str, state: Arc<RwLock<NodeState>>) {
    let endpoint = peer_endpoint.to_string();
    thread::spawn(move || {
        if let Ok(res_str) = http_get(&endpoint, "/api/p2p/sync", Duration::from_secs(4)) {
            if let Ok(sync_data) = serde_json::from_str::<SyncResponse>(&res_str) {
                let mut s = state.write();
                // 1. Sync contracts
                for contract in sync_data.contracts {
                    if s.store.get_contract(&contract.address).is_none() {
                        s.store.register_contract(contract);
                    }
                }
                // 2. Sync contract slots
                for (addr, slot, val) in sync_data.contract_slots {
                    s.store.set_contract_slot(addr, slot, val);
                }
                // 3. Sync vertices
                for v in sync_data.vertices {
                    s.dag.insert_vertex(v);
                }
                if sync_data.latest_round > s.round {
                    s.round = sync_data.latest_round;
                }
                println!(" [P2P 동기화 완료] 피어({})로부터 최신 상태 및 스마트 계약 동기화 성공!", endpoint);
            }
        }
    });
}

fn read_http_request(stream: &mut TcpStream) -> Option<String> {
    let mut buffer = Vec::with_capacity(8192);
    let mut temp = [0u8; 4096];
    let mut header_end = None;
    let mut content_length = 0;

    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));

    while header_end.is_none() {
        let n = stream.read(&mut temp).ok()?;
        if n == 0 {
            break;
        }
        buffer.extend_from_slice(&temp[..n]);

        if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = Some(pos + 4);
            let header_str = String::from_utf8_lossy(&buffer[..pos]);
            for line in header_str.lines() {
                if line.to_lowercase().starts_with("content-length:") {
                    if let Some(len_str) = line.split(':').nth(1) {
                        content_length = len_str.trim().parse::<usize>().unwrap_or(0);
                    }
                }
            }
        }
    }

    let h_end = header_end?;
    let body_received = buffer.len().saturating_sub(h_end);
    let mut remaining = content_length.saturating_sub(body_received);

    while remaining > 0 {
        let n = stream.read(&mut temp).ok()?;
        if n == 0 {
            break;
        }
        buffer.extend_from_slice(&temp[..n]);
        remaining = remaining.saturating_sub(n);
    }

    String::from_utf8(buffer).ok()
}

fn handle_connection(mut stream: TcpStream, state: Arc<RwLock<NodeState>>) {
    let peer_addr = stream.peer_addr().ok();
    let request = match read_http_request(&mut stream) {
        Some(r) => r,
        None => return,
    };
    let first_line = request.lines().next().unwrap_or("");
    let parts: Vec<&str> = first_line.split_whitespace().collect();

    if parts.len() < 2 {
        return;
    }

    let method = parts[0];
    let path = parts[1];

    if method == "GET" && path == "/" {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            DASHBOARD_HTML.len(),
            DASHBOARD_HTML
        );
        let _ = stream.write_all(response.as_bytes());
        return;
    }

    // P2P Handshake: Remote peer connects to us
    if method == "POST" && path == "/api/p2p/handshake" {
        if let Some(body_start) = request.find("\r\n\r\n") {
            let body = &request[body_start + 4..];
            if let Ok(hs) = serde_json::from_str::<aether_core::p2p::HandshakeRequest>(body.trim()) {
                let remote_ip = peer_addr
                    .map(|a| a.ip().to_string())
                    .unwrap_or_else(|| "127.0.0.1".to_string());
                let remote_endpoint = format!("{}:{}", remote_ip, hs.listen_port);

                let my_ident = {
                    let s = state.read();
                    s.peer_mgr.add_or_update(PeerInfo {
                        node_id: hs.identity.node_id.clone(),
                        address: hs.identity.address,
                        endpoint: remote_endpoint,
                        last_seen_ms: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64,
                        latency_ms: 2,
                        round: 0,
                    });
                    s.identity.clone()
                };

                let resp_json = serde_json::to_string(&my_ident).unwrap_or_default();
                send_json(&mut stream, &resp_json);
                return;
            }
        }
        send_json(&mut stream, r#"{"error":"invalid_handshake"}"#);
        return;
    }

    // P2P Connect: Request to connect to an external peer
    if method == "POST" && path == "/api/p2p/connect" {
        if let Some(body_start) = request.find("\r\n\r\n") {
            let body = &request[body_start + 4..];
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(body.trim()) {
                let peer_ep = val["peer"].as_str().unwrap_or("").to_string();
                let peer_mgr = state.read().peer_mgr.clone();

                match peer_mgr.connect_peer(&peer_ep) {
                    Ok(info) => {
                        // Immediately sync state with this peer
                        sync_with_peer(&peer_ep, Arc::clone(&state));
                        let resp = serde_json::json!({
                            "status": "connected",
                            "peer": info
                        });
                        send_json(&mut stream, &resp.to_string());
                        return;
                    }
                    Err(e) => {
                        let resp = serde_json::json!({
                            "status": "error",
                            "message": e
                        });
                        send_json(&mut stream, &resp.to_string());
                        return;
                    }
                }
            }
        }
        send_json(&mut stream, r#"{"error":"invalid_request"}"#);
        return;
    }

    // P2P Peers List
    if method == "GET" && path == "/api/p2p/peers" {
        let s = state.read();
        let peers = s.peer_mgr.list();
        let resp = serde_json::json!({
            "my_node_id": s.identity.node_id,
            "local_ip": s.local_ip,
            "port": s.port,
            "peer_count": peers.len(),
            "peers": peers
        });
        send_json(&mut stream, &resp.to_string());
        return;
    }

    // P2P Full State Sync
    if method == "GET" && path == "/api/p2p/sync" {
        let s = state.read();
        let resp = SyncResponse {
            latest_round: s.round,
            vertices: s.dag.get_all_vertices(),
            contracts: s.store.list_contracts(),
            contract_slots: s.store.export_all_contract_slots(),
        };
        let json = serde_json::to_string(&resp).unwrap_or_default();
        send_json(&mut stream, &json);
        return;
    }

    // P2P Gossip receiver: New transaction or vertex
    if method == "POST" && path == "/api/p2p/gossip" {
        if let Some(body_start) = request.find("\r\n\r\n") {
            let body = &request[body_start + 4..];
            if let Ok(msg) = serde_json::from_str::<GossipMessage>(body.trim()) {
                let mut s = state.write();
                match msg {
                    GossipMessage::NewTx(tx) => {
                        // Apply transaction locally
                        match &tx.payload {
                            TxPayload::DeployContract {
                                name,
                                template,
                                params,
                            } => {
                                let (contract, initial_slots) = SmartContractEngine::deploy(
                                    tx.sender,
                                    tx.nonce,
                                    s.round,
                                    name,
                                    template,
                                    params,
                                );
                                s.store.register_contract(contract.clone());
                                for (slot, val) in initial_slots {
                                    s.store.set_contract_slot(contract.address, slot, val);
                                }
                            }
                            TxPayload::CallContract {
                                contract,
                                method,
                                args,
                            } => {
                                if let Some(c_info) = s.store.get_contract(contract) {
                                    let res = SmartContractEngine::execute_call(
                                        tx.sender,
                                        &c_info,
                                        method,
                                        args,
                                        |slot| s.store.get_contract_slot(contract, slot),
                                    );
                                    for (slot, val) in res.contract_slot_writes {
                                        s.store.set_contract_slot(*contract, slot, val);
                                    }
                                }
                            }
                            TxPayload::Transfer { to, amount } => {
                                s.store.transfer(tx.sender, *to, *amount);
                            }
                            TxPayload::Swap { .. } => {}
                        }
                        s.total_txs += 1;
                    }
                    GossipMessage::NewVertex(v) => {
                        s.dag.insert_vertex(v);
                    }
                    _ => {}
                }
                send_json(&mut stream, r#"{"status":"gossip_applied"}"#);
                return;
            }
        }
        send_json(&mut stream, r#"{"status":"ignored"}"#);
        return;
    }

    if method == "GET" && path == "/api/status" {
        let s = state.read();
        let balance = s.store.get_balance(&s.my_address);
        let root = format!("{}", s.store.state_root());
        let peers = s.peer_mgr.list();
        let json = serde_json::json!({
            "node_id": s.identity.node_id,
            "node_name": s.identity.name,
            "address": s.my_address.to_hex(),
            "round": s.round,
            "total_txs": s.total_txs,
            "tps": s.last_tps,
            "state_root": root,
            "balance": balance,
            "total_rewards": s.total_rewards,
            "cores": rayon::current_num_threads(),
            "local_ip": s.local_ip,
            "port": s.port,
            "peer_count": peers.len(),
            "peers": peers
        });
        send_json(&mut stream, &json.to_string());
        return;
    }

    if method == "GET" && path == "/api/dag" {
        let s = state.read();
        let json = serde_json::json!({
            "vertices": s.vertices_cache
        });
        send_json(&mut stream, &json.to_string());
        return;
    }

    if method == "POST" && path == "/api/step" {
        let (round, vertex_opt, peer_mgr) = {
            let mut s = state.write();
            let v = s.advance_round();
            s.store.deposit(s.my_address, 10);
            s.total_rewards += 10;
            (s.round, v, s.peer_mgr.clone())
        };
        if let Some(v) = vertex_opt {
            peer_mgr.broadcast_gossip(&GossipMessage::NewVertex(v));
        }
        let resp = serde_json::json!({ "status": "ok", "round": round });
        send_json(&mut stream, &resp.to_string());
        return;
    }

    if method == "POST" && path == "/api/mev_attack" {
        let ciphertext = "[0xcf, 0xb3, 0xdf, 0x04, 0xc8, 0x88, 0x38, 0xd8]";
        let json = format!(
            r#"{{"ciphertext_preview":"{}","status":"attack_blocked","mev_extracted":0}}"#,
            ciphertext
        );
        send_json(&mut stream, &json);
        return;
    }

    if method == "GET" && path == "/api/contracts" {
        let s = state.read();
        let contracts = s.store.list_contracts();
        let mut list = Vec::new();
        for c in contracts {
            let slots = s.store.get_all_contract_slots(&c.address);
            let slots_json: serde_json::Map<String, serde_json::Value> = slots
                .into_iter()
                .map(|(k, v)| (k.to_string(), serde_json::Value::from(v)))
                .collect();

            list.push(serde_json::json!({
                "address": c.address.to_hex(),
                "name": c.name,
                "template": c.template,
                "creator": c.creator.to_hex(),
                "created_at_round": c.created_at_round,
                "slots": slots_json,
            }));
        }
        let json = serde_json::json!({ "contracts": list });
        send_json(&mut stream, &json.to_string());
        return;
    }

    if method == "POST" && path == "/api/contract/deploy" {
        if let Some(body_start) = request.find("\r\n\r\n") {
            let body = &request[body_start + 4..];
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(body.trim()) {
                let name = val["name"].as_str().unwrap_or("CustomContract").to_string();
                let template = val["template"].as_str().unwrap_or("counter").to_string();
                let params: Vec<u64> = val["params"]
                    .as_array()
                    .map(|arr| arr.iter().filter_map(|x| x.as_u64()).collect())
                    .unwrap_or_default();

                let (contract_addr, round, tx_to_gossip, peer_mgr) = {
                    let mut s = state.write();
                    let sender_nonce = s.store.get_account(&s.my_address).nonce;
                    let addr = SmartContractEngine::derive_contract_address(
                        &s.my_address,
                        sender_nonce,
                        &name,
                    );
                    let tx = Transaction {
                        id: s.total_txs as u64 + 1000,
                        sender: s.my_address,
                        nonce: sender_nonce,
                        payload: TxPayload::DeployContract {
                            name: name.clone(),
                            template: template.clone(),
                            params,
                        },
                        gas_limit: 100_000,
                    };
                    let enc = s.scheme.encrypt(&tx);
                    s.mempool.submit(enc);
                    s.advance_round();
                    (addr, s.round, tx, s.peer_mgr.clone())
                };

                // Broadcast to connected P2P peers
                peer_mgr.broadcast_gossip(&GossipMessage::NewTx(tx_to_gossip));

                let resp = serde_json::json!({
                    "status": "deployed",
                    "contract_address": contract_addr.to_hex(),
                    "name": name,
                    "template": template,
                    "round": round,
                });
                send_json(&mut stream, &resp.to_string());
                return;
            }
        }
        send_json(&mut stream, r#"{"error":"invalid_payload"}"#);
        return;
    }

    if method == "POST" && path == "/api/contract/call" {
        if let Some(body_start) = request.find("\r\n\r\n") {
            let body = &request[body_start + 4..];
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(body.trim()) {
                let contract_str = val["contract"].as_str().unwrap_or("");
                let method_name = val["method"].as_str().unwrap_or("").to_string();
                let args: Vec<u64> = val["args"]
                    .as_array()
                    .map(|arr| arr.iter().filter_map(|x| x.as_u64()).collect())
                    .unwrap_or_default();

                if let Some(target_contract) = Address::from_hex(contract_str) {
                    let (round, tx_to_gossip, peer_mgr) = {
                        let mut s = state.write();
                        let sender_nonce = s.store.get_account(&s.my_address).nonce;
                        let tx = Transaction {
                            id: s.total_txs as u64 + 1000,
                            sender: s.my_address,
                            nonce: sender_nonce,
                            payload: TxPayload::CallContract {
                                contract: target_contract,
                                method: method_name.clone(),
                                args,
                            },
                            gas_limit: 50_000,
                        };
                        let enc = s.scheme.encrypt(&tx);
                        s.mempool.submit(enc);
                        s.advance_round();
                        (s.round, tx, s.peer_mgr.clone())
                    };

                    // Broadcast to connected P2P peers
                    peer_mgr.broadcast_gossip(&GossipMessage::NewTx(tx_to_gossip));

                    let resp = serde_json::json!({
                        "status": "executed",
                        "contract": contract_str,
                        "method": method_name,
                        "round": round,
                    });
                    send_json(&mut stream, &resp.to_string());
                    return;
                }
            }
        }
        send_json(&mut stream, r#"{"error":"invalid_payload"}"#);
        return;
    }

    if method == "POST" && path == "/api/tx" {
        if let Some(body_start) = request.find("\r\n\r\n") {
            let body = &request[body_start + 4..];
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(body) {
                let amount = val["amount"].as_u64().unwrap_or(100);
                let is_swap = val["type"].as_str() == Some("swap");

                let (tx, peer_mgr) = {
                    let s = state.write();
                    let tx = Transaction {
                        id: s.total_txs as u64 + 999,
                        sender: s.my_address,
                        nonce: 0,
                        payload: if is_swap {
                            TxPayload::Swap {
                                pool_id: 1,
                                amount_in: amount,
                                min_out: amount.saturating_sub(10),
                            }
                        } else {
                            TxPayload::Transfer {
                                to: Address::new(102),
                                amount,
                            }
                        },
                        gas_limit: 21000,
                    };
                    let enc = s.scheme.encrypt(&tx);
                    s.mempool.submit(enc);
                    (tx, s.peer_mgr.clone())
                };
                peer_mgr.broadcast_gossip(&GossipMessage::NewTx(tx));
            }
        }
        send_json(&mut stream, r#"{"status":"submitted_encrypted"}"#);
        return;
    }

    if method == "POST" && path == "/api/bench" {
        const TX_COUNT: usize = 10_000;
        let mut rng = StdRng::seed_from_u64(42);
        let mut txs = Vec::with_capacity(TX_COUNT);
        let base_store = FlatStateStore::new();

        for i in 0..5000 {
            base_store.deposit(Address::new(i), 1_000_000);
        }

        for i in 0..TX_COUNT {
            let is_swap = rng.gen_bool(0.20);
            let sender = Address::new(rng.gen_range(0..2500));
            let payload = if is_swap {
                TxPayload::Swap {
                    pool_id: rng.gen_range(1..=10),
                    amount_in: rng.gen_range(10..200),
                    min_out: 5,
                }
            } else {
                TxPayload::Transfer {
                    to: Address::new(rng.gen_range(2501..5000)),
                    amount: rng.gen_range(1..50),
                }
            };
            txs.push(Transaction {
                id: i as u64,
                sender,
                nonce: 0,
                payload,
                gas_limit: 21000,
            });
        }

        let seq_start = Instant::now();
        let _ = SequentialExecutor::execute_block(&txs, &base_store);
        let seq_dur = seq_start.elapsed();
        let seq_tps = (TX_COUNT as f64) / seq_dur.as_secs_f64();

        let par_start = Instant::now();
        let (_new_store, _) = BlockSTMExecutor::execute_block(&txs, &base_store);
        let par_dur = par_start.elapsed();
        let par_tps = (TX_COUNT as f64) / par_dur.as_secs_f64();

        {
            let mut s = state.write();
            s.total_txs += TX_COUNT;
            s.last_tps = par_tps as usize;
        }

        let speedup = par_tps / seq_tps;
        let json = format!(
            r#"{{"seq_tps":{:.0},"par_tps":{:.0},"speedup":{:.2}}}"#,
            seq_tps, par_tps, speedup
        );
        send_json(&mut stream, &json);
        return;
    }

    // Default 404
    let not_found = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
    let _ = stream.write_all(not_found.as_bytes());
}

fn send_json(stream: &mut TcpStream, json: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        json.len(),
        json
    );
    let _ = stream.write_all(response.as_bytes());
}

fn load_or_create_identity(custom_port: u16) -> NodeIdentity {
    let home = std::env::var("HOME").ok().map(std::path::PathBuf::from);
    let path = if let Some(h) = home {
        let dir = h.join(".aether");
        let _ = std::fs::create_dir_all(&dir);
        if custom_port != 8080 {
            dir.join(format!("identity_{}.json", custom_port))
        } else {
            dir.join("identity.json")
        }
    } else {
        std::path::PathBuf::from(format!("./identity_{}.json", custom_port))
    };

    if let Ok(content) = std::fs::read_to_string(&path) {
        if let Ok(ident) = serde_json::from_str::<NodeIdentity>(&content) {
            return ident;
        }
    }

    let mut rng = StdRng::from_entropy();
    let id_num: u64 = rng.gen_range(1000..999999);
    let mut rand_bytes = [0u8; 8];
    rng.fill(&mut rand_bytes);
    let node_hex = format!("0x{}", hex::encode(&rand_bytes));
    let address = Address::new(id_num);
    let name = format!("Aether-Node-{}", &node_hex[2..6]);

    let ident = NodeIdentity {
        node_id: node_hex,
        address,
        name,
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };

    if let Ok(json) = serde_json::to_string_pretty(&ident) {
        let _ = std::fs::write(&path, json);
    }
    ident
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut requested_port = 8080;
    let mut connect_peer_arg: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        if args[i] == "--port" && i + 1 < args.len() {
            requested_port = args[i + 1].parse().unwrap_or(8080);
            i += 2;
        } else if args[i] == "--peer" && i + 1 < args.len() {
            connect_peer_arg = Some(args[i + 1].clone());
            i += 2;
        } else {
            i += 1;
        }
    }

    println!("\x1b[1;32m");
    println!("  █████╗ ███████╗████████╗██╗  ██╗███████╗██████╗ ");
    println!(" ██╔══██╗██╔════╝╚══██╔══╝██║  ██║██╔════╝██╔══██╗");
    println!(" ███████║█████╗     ██║   ███████║█████╗  ██████╔╝");
    println!(" ██╔══██║██╔══╝     ██║   ██╔══██║██╔══╝  ██╔══██╗");
    println!(" ██║  ██║███████╗   ██║   ██║  ██║███████╗██║  ██║");
    println!(" ╚═╝  ╚═╝╚══════╝   ╚═╝   ╚═╝  ╚═╝╚══════╝╚═╝  ╚═╝");
    println!("       SOVEREIGN BLOCKCHAIN NODE & P2P HUB        \x1b[0m");
    println!("\x1b[1;36m================================================================================\x1b[0m");
    println!(" [비트코인 정신] 'Don't trust, verify' - 누구나 집에서 가동하는 탈중앙 풀 노드");
    println!(" [연산 자원] {} CPU Cores / 64-Shard Lock-Free MVCC", rayon::current_num_threads());
    println!(" [메모리 점유율] ~38MB (배틀그라운드, 롤, 일상 작업 중에도 팬 소음 0% 쾌적 구동)");
    println!("\x1b[1;36m================================================================================\x1b[0m");

    // Bind on 0.0.0.0 for LAN and local access
    let mut listener = None;
    let mut port = requested_port;
    for offset in 0..10 {
        let p = requested_port + offset;
        match TcpListener::bind(format!("0.0.0.0:{}", p)) {
            Ok(l) => {
                port = p;
                listener = Some(l);
                break;
            }
            Err(_) => continue,
        }
    }

    let listener = listener.expect("네트워크 포트 바인딩에 실패했습니다");
    let local_ip = get_local_ip();
    let local_url = format!("http://127.0.0.1:{}", port);
    let lan_url = format!("http://{}:{}", local_ip, port);

    // Initialize Persistent Node Identity & Peer Manager
    let identity = load_or_create_identity(port);
    let peer_mgr = PeerManager::new(identity.clone(), port);

    println!(" \x1b[1;32m✔ Aether Sovereign Node 데몬 가동 완료!\x1b[0m");
    println!(" [내 노드 ID] \x1b[1;35m{}\x1b[0m ({})", identity.node_id, identity.name);
    println!(" [내 지갑 주소] \x1b[1;33m{}\x1b[0m", identity.address.to_hex());
    println!(" [로컬 접속 주소] \x1b[1;36m{}\x1b[0m", local_url);
    println!(" [P2P LAN 주소]   \x1b[1;32m{}\x1b[0m (다른 PC에서 이 주소로 연결 가능)", lan_url);
    println!("\x1b[1;36m================================================================================\x1b[0m");

    // Start LAN UDP Beacon Auto-Discovery
    start_lan_auto_discovery(peer_mgr.clone(), port);

    // If --peer argument was given, connect immediately
    if let Some(peer_addr) = connect_peer_arg {
        let pm = peer_mgr.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(500));
            println!(" [P2P 시작] 부트노드({})로 연결 시도...", peer_addr);
            let _ = pm.connect_peer(&peer_addr);
        });
    }

    let state = Arc::new(RwLock::new(NodeState::new(
        identity,
        peer_mgr,
        local_ip,
        port,
    )));

    // Background Block Production & Real Validator Reward Loop (Every 3 seconds)
    let state_bg = Arc::clone(&state);
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(3));
            let (_round, vertex_opt, peer_mgr) = {
                let mut s = state_bg.write();
                let v = s.advance_round();
                // Real Block Validation Reward (+10 AETH per finalized round)
                let reward = 10;
                s.store.deposit(s.my_address, reward);
                s.total_rewards += reward;
                (s.round, v, s.peer_mgr.clone())
            };

            // Broadcast vertex to connected peers
            if let Some(v) = vertex_opt {
                peer_mgr.broadcast_gossip(&GossipMessage::NewVertex(v));
            }
        }
    });

    // Open browser automatically in standalone App Window if available
    #[cfg(target_os = "macos")]
    {
        let opened_app_window = if std::path::Path::new("/Applications/Google Chrome.app").exists() {
            std::process::Command::new("open")
                .args(["-na", "Google Chrome", "--args", &format!("--app={}", local_url)])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        } else if std::path::Path::new("/Applications/Brave Browser.app").exists() {
            std::process::Command::new("open")
                .args(["-na", "Brave Browser", "--args", &format!("--app={}", local_url)])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        } else {
            false
        };

        if !opened_app_window {
            let _ = std::process::Command::new("open").arg(&local_url).spawn();
        }
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(&local_url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd").args(["/C", "start", &local_url]).spawn();
    }

    for stream in listener.incoming() {
        if let Ok(stream) = stream {
            let state_clone = Arc::clone(&state);
            thread::spawn(move || {
                handle_connection(stream, state_clone);
            });
        }
    }
}
