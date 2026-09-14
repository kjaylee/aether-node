use crate::types::{Address, ContractInfo};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub struct VmExecutionResult {
    pub contract_slot_writes: HashMap<u64, u64>,
    pub account_balance_deltas: Vec<(Address, i64)>,
    pub logs: Vec<String>,
    pub gas_used: u64,
}

pub struct SmartContractEngine;

impl SmartContractEngine {
    pub fn derive_contract_address(creator: &Address, nonce: u64, name: &str) -> Address {
        let mut hasher = Sha256::new();
        hasher.update(&creator.0);
        hasher.update(&nonce.to_be_bytes());
        hasher.update(name.as_bytes());
        let hash = hasher.finalize();
        let mut bytes = [0u8; 20];
        bytes.copy_from_slice(&hash[12..32]);
        Address(bytes)
    }

    pub fn deploy(
        creator: Address,
        nonce: u64,
        round: u64,
        name: &str,
        template: &str,
        params: &[u64],
    ) -> (ContractInfo, HashMap<u64, u64>) {
        let address = Self::derive_contract_address(&creator, nonce, name);
        let mut initial_slots = HashMap::new();

        match template.to_lowercase().as_str() {
            "counter" => {
                let init_val = params.get(0).copied().unwrap_or(0);
                initial_slots.insert(0, init_val); // Slot 0: Counter value
            }
            "token" => {
                let total_supply = params.get(0).copied().unwrap_or(1_000_000);
                initial_slots.insert(0, total_supply); // Slot 0: Total Supply
                let creator_id = u64::from_be_bytes(creator.0[12..20].try_into().unwrap_or_default());
                initial_slots.insert(100 + (creator_id % 1000), total_supply); // Slot: Creator balance
            }
            "vault" => {
                initial_slots.insert(0, 0); // Slot 0: Total Staked Assets
                initial_slots.insert(1, 0); // Slot 1: Total Shares Minted
                initial_slots.insert(2, 5); // Slot 2: APY % (Default 5%)
            }
            _ => {
                // Custom contract: initialize arbitrary slots from params
                for (i, &p) in params.iter().enumerate() {
                    initial_slots.insert(i as u64, p);
                }
            }
        }

        let info = ContractInfo {
            address,
            name: name.to_string(),
            template: template.to_lowercase(),
            creator,
            created_at_round: round,
        };

        (info, initial_slots)
    }

    pub fn execute_call<F>(
        sender: Address,
        contract: &ContractInfo,
        method: &str,
        args: &[u64],
        read_slot: F,
    ) -> VmExecutionResult
    where
        F: Fn(u64) -> u64,
    {
        let mut slot_writes = HashMap::new();
        let mut balance_deltas = Vec::new();
        let mut logs = Vec::new();
        let gas_used = 21000 + (args.len() as u64 * 500);

        let sender_id = u64::from_be_bytes(sender.0[12..20].try_into().unwrap_or_default());
        let sender_slot = 100 + (sender_id % 1000);

        match contract.template.as_str() {
            "counter" => {
                let current_val = read_slot(0);
                match method.to_lowercase().as_str() {
                    "increment" => {
                        let step = args.get(0).copied().unwrap_or(1);
                        let next = current_val.saturating_add(step);
                        slot_writes.insert(0, next);
                        logs.push(format!("Counter incremented: {} -> {}", current_val, next));
                    }
                    "decrement" => {
                        let step = args.get(0).copied().unwrap_or(1);
                        let next = current_val.saturating_sub(step);
                        slot_writes.insert(0, next);
                        logs.push(format!("Counter decremented: {} -> {}", current_val, next));
                    }
                    "set" => {
                        let val = args.get(0).copied().unwrap_or(0);
                        slot_writes.insert(0, val);
                        logs.push(format!("Counter set to: {}", val));
                    }
                    _ => {
                        logs.push(format!("Unknown counter method: {}", method));
                    }
                }
            }
            "token" => {
                match method.to_lowercase().as_str() {
                    "transfer" => {
                        let recipient_id = args.get(0).copied().unwrap_or(0);
                        let amount = args.get(1).copied().unwrap_or(0);
                        let recipient_slot = 100 + (recipient_id % 1000);

                        let sender_bal = read_slot(sender_slot);
                        let recv_bal = read_slot(recipient_slot);

                        if sender_bal >= amount {
                            slot_writes.insert(sender_slot, sender_bal - amount);
                            slot_writes.insert(recipient_slot, recv_bal + amount);
                            logs.push(format!("Transferred {} tokens from {} to {}", amount, sender, Address::new(recipient_id)));
                        } else {
                            logs.push(format!("Insufficient token balance: {} < {}", sender_bal, amount));
                        }
                    }
                    "mint" => {
                        let recipient_id = args.get(0).copied().unwrap_or(sender_id);
                        let amount = args.get(1).copied().unwrap_or(100);
                        let recipient_slot = 100 + (recipient_id % 1000);

                        let total_supply = read_slot(0);
                        let recv_bal = read_slot(recipient_slot);

                        slot_writes.insert(0, total_supply + amount);
                        slot_writes.insert(recipient_slot, recv_bal + amount);
                        logs.push(format!("Minted {} tokens to {}. New supply: {}", amount, Address::new(recipient_id), total_supply + amount));
                    }
                    _ => {
                        logs.push(format!("Unknown token method: {}", method));
                    }
                }
            }
            "vault" => {
                match method.to_lowercase().as_str() {
                    "deposit" => {
                        let amount = args.get(0).copied().unwrap_or(100);
                        let current_assets = read_slot(0);
                        let current_shares = read_slot(1);

                        // Transfer user balance to vault
                        balance_deltas.push((sender, -(amount as i64)));

                        let shares_to_mint = if current_assets == 0 || current_shares == 0 {
                            amount
                        } else {
                            (amount * current_shares) / current_assets
                        };

                        let user_shares = read_slot(sender_slot);

                        slot_writes.insert(0, current_assets + amount);
                        slot_writes.insert(1, current_shares + shares_to_mint);
                        slot_writes.insert(sender_slot, user_shares + shares_to_mint);

                        logs.push(format!("Vault deposit: {} AETH deposited, {} shares minted for {}", amount, shares_to_mint, sender));
                    }
                    "compound" => {
                        let apy = read_slot(2);
                        let current_assets = read_slot(0);
                        let yield_amount = (current_assets * apy) / 100;
                        slot_writes.insert(0, current_assets + yield_amount);
                        logs.push(format!("Vault compounded: +{} yield generated. Total assets: {}", yield_amount, current_assets + yield_amount));
                    }
                    _ => {
                        logs.push(format!("Unknown vault method: {}", method));
                    }
                }
            }
            _ => {
                // Custom program execution: write args directly as (slot, value) pairs
                for i in (0..args.len()).step_by(2) {
                    if i + 1 < args.len() {
                        let slot = args[i];
                        let val = args[i + 1];
                        slot_writes.insert(slot, val);
                        logs.push(format!("Custom contract: Slot[{}] = {}", slot, val));
                    }
                }
            }
        }

        VmExecutionResult {
            contract_slot_writes: slot_writes,
            account_balance_deltas: balance_deltas,
            logs,
            gas_used,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_counter_lifecycle() {
        let creator = Address::new(1);
        let (info, initial_slots) = SmartContractEngine::deploy(creator, 0, 1, "MyCounter", "counter", &[10]);
        assert_eq!(initial_slots.get(&0), Some(&10));

        let mut storage = initial_slots;
        let res = SmartContractEngine::execute_call(
            creator,
            &info,
            "increment",
            &[5],
            |slot| *storage.get(&slot).unwrap_or(&0),
        );
        assert_eq!(res.contract_slot_writes.get(&0), Some(&15));
        storage.extend(res.contract_slot_writes);

        let res2 = SmartContractEngine::execute_call(
            creator,
            &info,
            "decrement",
            &[3],
            |slot| *storage.get(&slot).unwrap_or(&0),
        );
        assert_eq!(res2.contract_slot_writes.get(&0), Some(&12));
    }

    #[test]
    fn test_token_lifecycle() {
        let creator = Address::new(10);
        let (info, initial_slots) = SmartContractEngine::deploy(creator, 0, 1, "GovToken", "token", &[1_000_000]);
        assert_eq!(initial_slots.get(&0), Some(&1_000_000));

        let mut storage = initial_slots;
        // Mint to user 20
        let res = SmartContractEngine::execute_call(
            creator,
            &info,
            "mint",
            &[20, 500],
            |slot| *storage.get(&slot).unwrap_or(&0),
        );
        storage.extend(res.contract_slot_writes);
        assert_eq!(storage.get(&0), Some(&1_000_500));
        assert_eq!(storage.get(&(100 + 20)), Some(&500));
    }

    #[test]
    fn test_custom_contract() {
        let creator = Address::new(99);
        let (info, initial_slots) = SmartContractEngine::deploy(creator, 0, 1, "CustomLogic", "custom", &[111, 222, 333]);
        assert_eq!(initial_slots.get(&0), Some(&111));
        assert_eq!(initial_slots.get(&1), Some(&222));
        assert_eq!(initial_slots.get(&2), Some(&333));

        let mut storage = initial_slots;
        let res = SmartContractEngine::execute_call(
            creator,
            &info,
            "set_slots",
            &[5, 9999], // Slot 5 = 9999
            |slot| *storage.get(&slot).unwrap_or(&0),
        );
        storage.extend(res.contract_slot_writes);
        assert_eq!(storage.get(&5), Some(&9999));
    }
}

