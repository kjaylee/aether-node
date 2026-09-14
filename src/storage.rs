use crate::types::{AccountState, Address, Hash256};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct FlatStateStore {
    accounts: Arc<RwLock<HashMap<Address, AccountState>>>,
    storage_slots: Arc<RwLock<HashMap<(Address, Hash256), u64>>>,
}

impl FlatStateStore {
    pub fn new() -> Self {
        FlatStateStore {
            accounts: Arc::new(RwLock::new(HashMap::new())),
            storage_slots: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn get_account(&self, addr: &Address) -> AccountState {
        self.accounts.read().get(addr).cloned().unwrap_or_default()
    }

    pub fn get_balance(&self, addr: &Address) -> u64 {
        self.get_account(addr).balance
    }

    pub fn set_account(&self, addr: Address, state: AccountState) {
        self.accounts.write().insert(addr, state);
    }

    pub fn deposit(&self, addr: Address, amount: u64) {
        let mut w = self.accounts.write();
        let entry = w.entry(addr).or_default();
        entry.balance += amount;
    }

    pub fn get_slot(&self, addr: &Address, slot: &Hash256) -> u64 {
        self.storage_slots.read().get(&(*addr, *slot)).cloned().unwrap_or(0)
    }

    pub fn set_slot(&self, addr: Address, slot: Hash256, val: u64) {
        self.storage_slots.write().insert((addr, slot), val);
    }

    pub fn apply_batch(&self, account_writes: HashMap<Address, AccountState>, slot_writes: HashMap<(Address, Hash256), u64>) {
        let mut acc_w = self.accounts.write();
        for (addr, state) in account_writes {
            acc_w.insert(addr, state);
        }

        let mut slot_w = self.storage_slots.write();
        for (k, v) in slot_writes {
            slot_w.insert(k, v);
        }
    }

    pub fn state_root(&self) -> Hash256 {
        let r = self.accounts.read();
        let mut sorted_keys: Vec<_> = r.keys().collect();
        sorted_keys.sort_by_key(|a| a.0);

        let mut bytes = Vec::new();
        for k in sorted_keys {
            bytes.extend_from_slice(&k.0);
            let state = &r[k];
            bytes.extend_from_slice(&state.balance.to_be_bytes());
            bytes.extend_from_slice(&state.nonce.to_be_bytes());
        }
        Hash256::of(&bytes)
    }
}
