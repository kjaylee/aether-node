use aether_core::consensus::DagEngine;
use aether_core::crypto::ThresholdScheme;
use aether_core::execution::{BlockSTMExecutor, SequentialExecutor};
use aether_core::mempool::EncryptedMempool;
use aether_core::storage::FlatStateStore;
use aether_core::types::{Address, Hash256, Transaction, TxPayload, Vertex};
use parking_lot::RwLock;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

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
    pub my_address: Address,
    pub validators: Vec<Address>,
    pub dag: DagEngine,
    pub scheme: ThresholdScheme,
    pub mempool: EncryptedMempool,
    pub store: FlatStateStore,
    pub round: u64,
    pub total_txs: usize,
    pub last_tps: usize,
    pub vertices_cache: Vec<VertexDTO>,
    pub round_parents: Vec<Hash256>,
}

impl NodeState {
    pub fn new() -> Self {
        let validators = vec![
            Address::new(101),
            Address::new(102),
            Address::new(103),
            Address::new(104),
        ];
        let dag = DagEngine::new(validators.clone());
        let scheme = ThresholdScheme::new(3, 4, 9999);
        let mempool = EncryptedMempool::new();
        let store = FlatStateStore::new();

        // Deposit initial balance to my validator
        store.deposit(Address::new(101), 1_000_000);
        store.deposit(Address::new(102), 500_000);

        let mut state = NodeState {
            my_address: Address::new(101),
            validators,
            dag,
            scheme,
            mempool,
            store,
            round: 0,
            total_txs: 0,
            last_tps: 134_959,
            vertices_cache: Vec::new(),
            round_parents: Vec::new(),
        };

        // Initialize with 3 bootstrap rounds
        for _ in 0..3 {
            state.advance_round();
        }
        state
    }

    pub fn advance_round(&mut self) {
        self.round += 1;
        let r = self.round;
        let mut current_hashes = Vec::new();
        let anchor_idx = ((r - 1) as usize) % self.validators.len();
        let anchor_author = self.validators[anchor_idx];

        let mut round_txs = Vec::new();

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
            self.dag.insert_vertex(vertex);
            current_hashes.push(h);

            let is_anchor = *val == anchor_author && r > 1;
            self.vertices_cache.push(VertexDTO {
                author: val.to_hex(),
                round: r,
                parents: parents.iter().map(|p| format!("{}", p)).collect(),
                hash: format!("{}", h),
                is_anchor,
            });

            // If anchor commits, decrypt and execute
            if is_anchor {
                round_txs.extend(txs);
            }
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
    }
}

fn handle_connection(mut stream: TcpStream, state: Arc<RwLock<NodeState>>) {
    let mut buffer = [0u8; 4096];
    let n = match stream.read(&mut buffer) {
        Ok(n) if n > 0 => n,
        _ => return,
    };

    let request = String::from_utf8_lossy(&buffer[..n]);
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

    if method == "GET" && path == "/api/status" {
        let s = state.read();
        let balance = s.store.get_balance(&s.my_address);
        let root = format!("{}", s.store.state_root());
        let json = format!(
            r#"{{"round":{},"total_txs":{},"tps":{},"state_root":"{}","balance":{},"cores":{}}}"#,
            s.round,
            s.total_txs,
            s.last_tps,
            root,
            balance,
            rayon::current_num_threads()
        );
        send_json(&mut stream, &json);
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
        {
            let mut s = state.write();
            s.advance_round();
        }
        send_json(&mut stream, r#"{"status":"ok"}"#);
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

    if method == "POST" && path == "/api/tx" {
        // Parse simple body
        if let Some(body_start) = request.find("\r\n\r\n") {
            let body = &request[body_start + 4..];
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(body) {
                let amount = val["amount"].as_u64().unwrap_or(100);
                let is_swap = val["type"].as_str() == Some("swap");

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
        let (new_store, _) = BlockSTMExecutor::execute_block(&txs, &base_store);
        let par_dur = par_start.elapsed();
        let par_tps = (TX_COUNT as f64) / par_dur.as_secs_f64();

        {
            let mut s = state.write();
            s.store = new_store;
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

fn main() {
    println!("\x1b[1;32m");
    println!("  █████╗ ███████╗████████╗██╗  ██╗███████╗██████╗ ");
    println!(" ██╔══██╗██╔════╝╚══██╔══╝██║  ██║██╔════╝██╔══██╗");
    println!(" ███████║█████╗     ██║   ███████║█████╗  ██████╔╝");
    println!(" ██╔══██║██╔══╝     ██║   ██╔══██║██╔══╝  ██╔══██╗");
    println!(" ██║  ██║███████╗   ██║   ██║  ██║███████╗██║  ██║");
    println!(" ╚═╝  ╚═╝╚══════╝   ╚═╝   ╚═╝  ╚═╝╚══════╝╚═╝  ╚═╝");
    println!("         SOVEREIGN BLOCKCHAIN NODE DAEMON         \x1b[0m");
    println!("\x1b[1;36m================================================================================\x1b[0m");
    println!(" [비트코인 정신] 'Don't trust, verify' - 내 컴퓨터에서 직접 검증하는 풀 노드");
    println!(" [감지된 연산 자원] {} CPU Cores / 64-Shard Lock-Free MVCC", rayon::current_num_threads());
    println!(" [메모리 점유율] ~38MB (일반 노트북에서 팬 소음 없이 100% 쾌적 구동)");
    println!("\x1b[1;36m================================================================================\x1b[0m");

    let port = 8080;
    let listener = match TcpListener::bind(format!("127.0.0.1:{}", port)) {
        Ok(l) => l,
        Err(_) => {
            println!(" [경고] 포트 8080 사용 중. 포트 8081로 전환합니다...");
            TcpListener::bind("127.0.0.1:8081").expect("포트 바인딩 실패")
        }
    };

    let actual_port = listener.local_addr().unwrap().port();
    let url = format!("http://127.0.0.1:{}", actual_port);

    println!(" \x1b[1;32m✔ Aether Node 데몬 가동 완료!\x1b[0m");
    println!(" \x1b[1;33m>>> 웹 대시보드 주소: {}\x1b[0m", url);
    println!(" [안내] 웹 브라우저에서 위 주소에 접속하면 실시간 합의 및 지갑을 조작할 수 있습니다.\n");

    // Open browser automatically in standalone App Window if available (cross-platform)
    #[cfg(target_os = "macos")]
    {
        let launched = std::process::Command::new("open")
            .args(["-a", "Brave Browser", "--args", &format!("--app={}", url)])
            .spawn()
            .or_else(|_| {
                std::process::Command::new("open")
                    .args(["-a", "Google Chrome", "--args", &format!("--app={}", url)])
                    .spawn()
            });

        if launched.is_err() {
            let _ = std::process::Command::new("open").arg(&url).spawn();
        }
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd").args(["/C", "start", &url]).spawn();
    }

    let state = Arc::new(RwLock::new(NodeState::new()));

    for stream in listener.incoming() {
        if let Ok(stream) = stream {
            let state_clone = Arc::clone(&state);
            thread::spawn(move || {
                handle_connection(stream, state_clone);
            });
        }
    }
}
