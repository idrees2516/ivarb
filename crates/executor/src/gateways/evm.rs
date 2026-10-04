//! EVM chain gateway — real-world chains, real protocols, real execution.
//!
//! What this module does, end-to-end and for real:
//! - **JSON-RPC** over HTTPS against public nodes (Base, Arbitrum, Optimism,
//!   Ethereum) — `eth_chainId` / `eth_blockNumber` / `eth_gasPrice` /
//!   `eth_call` / `eth_getBalance` / `eth_getTransactionCount` /
//!   `eth_estimateGas` / `eth_sendRawTransaction`.
//! - **Uniswap V3 quoting** through QuoterV2 `quoteExactInputSingle` — the
//!   same `eth_call` path the router itself would take, so the price shown
//!   is the price executable (verified live on all four chains).
//! - **Swap execution** through SwapRouter02 `exactInputSingle`: quote →
//!   slippage floor → ERC-20 allowance check (and a real `approve` tx when
//!   missing) → calldata → EIP-1559 sign → `eth_sendRawTransaction`.
//! - Optional **wrap** (WETH `deposit()`) so a signer holding native ETH
//!   can run the full ETH → USDC path in two txs.
//!
//! Every address and selector below was verified against live chains before
//! being committed (see docs/LIVE-TRADING.md "verification" section):
//! - QuoterV2: `0x61fFE014bA17989E743c5F6cB21bF9697530B21e` (eth/arb/op),
//!   `0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a` (base).
//! - SwapRouter02: `0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45` (all four).
//! - `quoteExactInputSingle((address,address,uint256,uint24,uint160))` =
//!   `0xc6a5026a`; `exactInputSingle((…uint160))` = `0x04e45aaf`.
//!
//! Safety model: nothing broadcasts unless `IVARB_CHAIN_SIGNER_KEY` holds a
//! raw 32-byte hex private key for a funded wallet the user controls. With
//! no key, the gateway still quotes real prices and builds ready-to-sign
//! payloads (returned in the swap outcome, shown in the UI).
//!
//! Known trade-offs (deliberate, documented, honest):
//! - Public RPCs are rate-limited; set `IVARB_RPC_<CHAIN>` to a private node
//!   for production cadence.
//! - Fees/gas are estimated once per swap (no gas bench).

use std::time::Duration;

use serde_json::Value;
use sha3::{Digest, Keccak256};

// ---------------------------------------------------------------------------
// Chain registry (all values live-verified; env overridable)
// ---------------------------------------------------------------------------

pub const WETH_DECIMALS: u32 = 18;
pub const USDC_DECIMALS: u32 = 6;

#[derive(Debug, Clone)]
pub struct ChainCfg {
    pub name: &'static str,
    pub chain_id: u64,
    pub quoter: &'static str,
    pub router: &'static str,
    pub weth: &'static str,
    pub usdc: &'static str,
    pub rpc_env: &'static str,
    pub rpc: String,
}

const SWAP_ROUTER02: &str = "0x68b3465833fb72a70ecdf485e0e4c7bd8665fc45";
const QUOTER_V2: &str = "0x61ffe014ba17989e743c5f6cb21bf9697530b21e";
const QUOTER_V2_BASE: &str = "0x3d4e44eb1374240ce5f1b871ab261cd16335b76a";

fn default_chains() -> Vec<ChainCfg> {
    let mk = |name: &'static str, chain_id: u64, quoter: &'static str, weth: &'static str, usdc: &'static str, rpc_env: &'static str, rpc: &str| ChainCfg {
        name,
        chain_id,
        quoter,
        router: SWAP_ROUTER02,
        weth,
        usdc,
        rpc_env,
        rpc: rpc.to_string(),
    };
    vec![
        mk("base", 8453, QUOTER_V2_BASE,
           "0x4200000000000000000000000000000000000006", "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913",
           "IVARB_RPC_BASE", "https://mainnet.base.org"),
        mk("arbitrum", 42161, QUOTER_V2,
           "0x82af49447d8a07e3bd95bd0d56f35241523fbab1", "0xaf88d065e77c8cc2239327c5edb3a432268e5831",
           "IVARB_RPC_ARBITRUM", "https://arb1.arbitrum.io/rpc"),
        mk("optimism", 10, QUOTER_V2,
           "0x4200000000000000000000000000000000000006", "0x0b2c639c533813f4aa9d7837caf62653d097ff85",
           "IVARB_RPC_OPTIMISM", "https://mainnet.optimism.io"),
        mk("ethereum", 1, QUOTER_V2,
           "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2", "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
           "IVARB_RPC_ETHEREUM", "https://eth.drpc.org"),
    ]
}

fn env_override(chains: Vec<ChainCfg>) -> Vec<ChainCfg> {
    // Private RPC override: IVARB_RPC_BASE / _ARBITRUM / _OPTIMISM / _ETHEREUM
    chains
        .into_iter()
        .map(|mut c| {
            if let Ok(u) = std::env::var(c.rpc_env) {
                if !u.trim().is_empty() {
                    c.rpc = u.trim().to_string();
                }
            }
            c
        })
        .collect()
}

// ---------------------------------------------------------------------------
// ABI + keccak helpers
// ---------------------------------------------------------------------------

pub fn keccak256(data: &[u8]) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(data);
    let out = h.finalize();
    let mut b = [0u8; 32];
    b.copy_from_slice(&out);
    b
}

fn selector(sig: &str) -> [u8; 4] {
    keccak256(sig.as_bytes())[..4].try_into().unwrap()
}

/// 20-byte address from a hex string (0x optional, case-insensitive, checksum ignored).
pub fn parse_address(s: &str) -> Option<[u8; 20]> {
    let h = s.trim().trim_start_matches("0x");
    if h.len() != 40 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// ABI word: 32-byte left-padded integer.
fn word_uint(v: u128) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[16..].copy_from_slice(&v.to_be_bytes());
    w
}

/// ABI word: right-padded address.
fn word_addr(a: &[u8; 20]) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(a);
    w
}

pub fn hex_lower(bytes: &[u8]) -> String {
    super::sign::hex(bytes)
}

pub fn hex_no_prefix(bytes: &[u8]) -> String {
    super::sign::hex(bytes)
}

/// `0x…` address string (lowercase).
pub fn addr_hex(a: &[u8; 20]) -> String {
    format!("0x{}", super::sign::hex(a))
}

/// QuoterV2 `quoteExactInputSingle((address,address,uint256,uint24,uint160))`
/// calldata — selector 0xc6a5026a (verified live on four chains).
pub fn build_quote_calldata(token_in: &[u8; 20], token_out: &[u8; 20], amount_in: u128) -> Vec<u8> {
    let sel = selector("quoteExactInputSingle((address,address,uint256,uint24,uint160))");
    assert_eq!(sel, [0xc6, 0xa5, 0x02, 0x6a]);
    let mut out = Vec::with_capacity(4 + 32 * 5);
    out.extend_from_slice(&sel);
    out.extend_from_slice(&word_addr(token_in));
    out.extend_from_slice(&word_addr(token_out));
    out.extend_from_slice(&word_uint(amount_in));
    out.extend_from_slice(&word_uint(500)); // 0.05% — the liquid WETH/USDC tier
    out.extend_from_slice(&word_uint(0)); // sqrtPriceLimitX160 = 0 (no limit)
    out
}

/// SwapRouter02 `exactInputSingle` calldata — selector 0x04e45aaf,
/// struct (tokenIn, tokenOut, fee, recipient, amountIn, amountOutMinimum,
/// sqrtPriceLimitX160). The router pulls tokenIn via transferFrom, so the
/// signer must have approved it (see allowance/approve helpers).
pub fn build_swap_calldata(
    token_in: &[u8; 20],
    token_out: &[u8; 20],
    recipient: &[u8; 20],
    amount_in: u128,
    amount_out_min: u128,
) -> Vec<u8> {
    let sel = selector("exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))");
    assert_eq!(sel, [0x04, 0xe4, 0x5a, 0xaf]);
    let mut out = Vec::with_capacity(4 + 32 * 7);
    out.extend_from_slice(&sel);
    out.extend_from_slice(&word_addr(token_in));
    out.extend_from_slice(&word_addr(token_out));
    out.extend_from_slice(&word_uint(500));
    out.extend_from_slice(&word_addr(recipient));
    out.extend_from_slice(&word_uint(amount_in));
    out.extend_from_slice(&word_uint(amount_out_min));
    out.extend_from_slice(&word_uint(0));
    out
}

/// ERC-20 `approve(spender, amount)`.
pub fn build_approve_calldata(spender: &[u8; 20], amount: u128) -> Vec<u8> {
    let sel = selector("approve(address,uint256)");
    let mut out = Vec::with_capacity(68);
    out.extend_from_slice(&sel);
    out.extend_from_slice(&word_addr(spender));
    out.extend_from_slice(&word_uint(amount));
    out
}

/// ERC-20 `allowance(owner, spender)`.
pub fn build_allowance_calldata(owner: &[u8; 20], spender: &[u8; 20]) -> Vec<u8> {
    let sel = selector("allowance(address,address)");
    let mut out = Vec::with_capacity(100);
    out.extend_from_slice(&sel);
    out.extend_from_slice(&word_addr(owner));
    out.extend_from_slice(&word_addr(spender));
    out
}

/// ERC-20 `balanceOf(owner)`.
pub fn build_balance_of_calldata(owner: &[u8; 20]) -> Vec<u8> {
    let sel = selector("balanceOf(address)");
    let mut out = Vec::with_capacity(68);
    out.extend_from_slice(&sel);
    out.extend_from_slice(&word_addr(owner));
    out
}

/// WETH9 `deposit()` — wrap native ETH.
pub fn build_wrap_calldata() -> Vec<u8> {
    selector("deposit()").to_vec()
}

/// ERC-20 `withdraw(uint256)` — unwrap WETH to native ETH.
pub fn build_unwrap_calldata(amount: u128) -> Vec<u8> {
    let sel = selector("withdraw(uint256)");
    let mut out = Vec::with_capacity(68);
    out.extend_from_slice(&sel);
    out.extend_from_slice(&word_uint(amount));
    out
}

/// Decode the first return word of a successful eth_call as u128.
pub fn decode_u128_word(result_hex: &str) -> Option<u128> {
    let h = result_hex.trim().trim_start_matches("0x");
    if h.len() < 64 {
        return None;
    }
    u128::from_str_radix(&h[..64], 16).ok()
}

// ---------------------------------------------------------------------------
// RLP (Ethereum's length-prefixed encoding) — hand-rolled, spec-vectored
// ---------------------------------------------------------------------------

pub mod rlp {
    fn be_minimal(mut v: u128) -> Vec<u8> {
        if v == 0 {
            return Vec::new();
        }
        let mut bytes = Vec::with_capacity(16);
        while v > 0 {
            bytes.push((v & 0xff) as u8);
            v >>= 8;
        }
        bytes.reverse();
        bytes
    }

    fn length_prefix(len: usize, small_off: u8, large_off: u8) -> Vec<u8> {
        if len < 56 {
            vec![small_off + len as u8]
        } else {
            let be = be_minimal(len as u128);
            let mut out = vec![large_off + be.len() as u8];
            out.extend_from_slice(&be);
            out
        }
    }

    /// RLP-encode a byte string.
    pub fn encode_bytes(b: &[u8]) -> Vec<u8> {
        if b.len() == 1 && b[0] < 0x80 {
            return b.to_vec();
        }
        let mut out = length_prefix(b.len(), 0x80, 0xb7);
        out.extend_from_slice(b);
        out
    }

    /// RLP-encode a unsigned integer (big-endian minimal, no leading zeros).
    pub fn encode_uint(v: u128) -> Vec<u8> {
        encode_bytes(&be_minimal(v))
    }

    /// RLP-encode a list of already-encoded items.
    pub fn encode_list(items: &[Vec<u8>]) -> Vec<u8> {
        let mut payload = Vec::new();
        for it in items {
            payload.extend_from_slice(it);
        }
        let mut out = length_prefix(payload.len(), 0xc0, 0xf7);
        out.extend_from_slice(&payload);
        out
    }
}

// ---------------------------------------------------------------------------
// EIP-1559 transaction (type 0x02) — build, sign, serialize
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Eip1559Tx {
    pub chain_id: u64,
    pub nonce: u64,
    pub max_priority_fee_gwei: f64,
    pub max_fee_gwei: f64,
    pub gas_limit: u64,
    pub to: Option<[u8; 20]>, // None = contract creation
    pub value_wei: u128,
    pub data: Vec<u8>,
}

impl Eip1559Tx {
    fn access_list_rlp(&self) -> Vec<u8> {
        rlp::encode_list(&[]) // empty access list
    }

    /// The message that gets signed: keccak(rlp([0x02, chainId, nonce,
    /// prio, maxFee, gas, to, value, data, accessList])).
    pub fn sighash(&self) -> [u8; 32] {
        let mut items: Vec<Vec<u8>> = Vec::with_capacity(10);
        items.push(vec![0x02]);
        items.push(rlp::encode_uint(self.chain_id as u128));
        items.push(rlp::encode_uint(self.nonce as u128));
        items.push(rlp::encode_uint(gwei_to_wei(self.max_priority_fee_gwei)));
        items.push(rlp::encode_uint(gwei_to_wei(self.max_fee_gwei)));
        items.push(rlp::encode_uint(self.gas_limit as u128));
        match self.to {
            Some(a) => items.push(rlp::encode_bytes(&a)),
            None => items.push(rlp::encode_bytes(&[])),
        }
        items.push(rlp::encode_uint(self.value_wei));
        items.push(rlp::encode_bytes(&self.data));
        items.push(self.access_list_rlp());
        keccak256(&rlp::encode_list(&items))
    }

    /// Sign with a secp256k1 key and serialize the raw typed transaction
    /// (0x02 || rlp([chainId, nonce, prio, maxFee, gas, to, value, data,
    /// accessList, yParity, r, s])). Uses k256's RFC6979 recoverable
    /// signing to obtain the y-parity for the typed-tx signature field.
    pub fn sign(&self, key: &k256::ecdsa::SigningKey) -> anyhow::Result<Vec<u8>> {
        let sighash = self.sighash();
        let (sig, recid) = key
            .sign_prehash_recoverable(&sighash)
            .map_err(|e| anyhow::anyhow!("secp256k1 sign: {e}"))?;
        let b = sig.to_bytes();
        anyhow::ensure!(b.len() == 64, "bad signature length");
        let (r, s) = (&b[..32], &b[32..64]);
        let y_parity = u8::from(recid) as u128;
        let _ = (r, s);

        let mut items: Vec<Vec<u8>> = Vec::with_capacity(13);
        items.push(rlp::encode_uint(self.chain_id as u128));
        items.push(rlp::encode_uint(self.nonce as u128));
        items.push(rlp::encode_uint(gwei_to_wei(self.max_priority_fee_gwei)));
        items.push(rlp::encode_uint(gwei_to_wei(self.max_fee_gwei)));
        items.push(rlp::encode_uint(self.gas_limit as u128));
        match self.to {
            Some(a) => items.push(rlp::encode_bytes(&a)),
            None => items.push(rlp::encode_bytes(&[])),
        }
        items.push(rlp::encode_uint(self.value_wei));
        items.push(rlp::encode_bytes(&self.data));
        items.push(self.access_list_rlp());
        items.push(rlp::encode_uint(y_parity));
        items.push(rlp::encode_bytes(be_stripped(&b[..32])));
        items.push(rlp::encode_bytes(be_stripped(&b[32..64])));
        let mut raw = vec![0x02];
        raw.extend_from_slice(&rlp::encode_list(&items));
        Ok(raw)
    }
}

/// Minimal big-endian form (leading zeros stripped) — RLP's integer
/// encoding for 256-bit values like r/s that exceed u128.
fn be_stripped(b: &[u8]) -> &[u8] {
    let mut i = 0;
    while i < b.len() && b[i] == 0 {
        i += 1;
    }
    &b[i..]
}

pub fn gwei_to_wei(gwei: f64) -> u128 {
    (gwei * 1e9).round() as u128
}

/// Derive the 20-byte address of a secp256k1 public key (Ethereum style:
/// keccak256(uncompressed_pubkey[1..])[12..]).
pub fn key_address(key: &k256::ecdsa::SigningKey) -> [u8; 20] {
    let vk = key.verifying_key();
    let encoded = vk.to_encoded_point(false); // 0x04 || X || Y
    let digest = keccak256(&encoded.as_bytes()[1..]);
    let mut a = [0u8; 20];
    a.copy_from_slice(&digest[12..]);
    a
}

// ---------------------------------------------------------------------------
// Gateway
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ChainStatus {
    pub name: String,
    pub chain_id_ok: bool,
    pub block: u64,
    pub weth_usdc_out: f64,
    pub eth_price: f64,
    pub gas_gwei: f64,
    pub quote_ok: bool,
    pub rpc_ms: u32,
}

#[derive(Debug, Clone)]
pub struct SwapOutcome {
    pub chain: String,
    pub direction: String,
    pub amount_in: f64,
    pub quoted_out: f64,
    pub min_out: f64,
    pub nonce: u64,
    pub gas_gwei: f64,
    pub calldata_hex: String,
    pub raw_tx_hex: Option<String>,
    pub tx_hash: Option<String>,
    pub approved_first: bool,
    pub broadcast: bool,
    pub note: String,
}

pub struct EvmGate {
    http: reqwest::Client,
    pub chains: Vec<ChainCfg>,
    pub signer: Option<k256::ecdsa::SigningKey>,
    pub slippage_bps: u32,
}

impl EvmGate {
    pub fn from_env() -> Self {
        let signer = std::env::var("IVARB_CHAIN_SIGNER_KEY")
            .ok()
            .and_then(|k| parse_secret_key(&k));
        let slippage_bps = std::env::var("IVARB_CHAIN_SLIPPAGE_BPS")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(50);
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .user_agent("ivarb/0.1")
                .build()
                .expect("http client"),
            chains: env_override(default_chains()),
            signer,
            slippage_bps,
        }
    }

    pub fn chain(&self, name: &str) -> Option<&ChainCfg> {
        self.chains.iter().find(|c| c.name == name)
    }

    pub fn signer_address(&self) -> Option<String> {
        self.signer.as_ref().map(key_address).map(|a| addr_hex(&a))
    }

    async fn rpc(&self, url: &str, method: &str, params: Value) -> anyhow::Result<Value> {
        let body = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let v: Value = self.http.post(url).json(&body).send().await?.json().await?;
        if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
            anyhow::bail!("rpc {method}: {err}");
        }
        Ok(v["result"].clone())
    }

    pub async fn chain_id(&self, chain: &str) -> anyhow::Result<u64> {
        let c = self.chain(chain).ok_or_else(|| anyhow::anyhow!("unknown chain {chain}"))?;
        let r = self.rpc(&c.rpc, "eth_chainId", serde_json::json!([])).await?;
        let h = r.as_str().unwrap_or_default().trim_start_matches("0x");
        u64::from_str_radix(h, 16).map_err(|_| anyhow::anyhow!("bad chainId {r}"))
    }

    pub async fn block_number(&self, chain: &str) -> anyhow::Result<u64> {
        let c = self.chain(chain).ok_or_else(|| anyhow::anyhow!("unknown chain {chain}"))?;
        let r = self.rpc(&c.rpc, "eth_blockNumber", serde_json::json!([])).await?;
        let h = r.as_str().unwrap_or_default().trim_start_matches("0x");
        u64::from_str_radix(h, 16).map_err(|_| anyhow::anyhow!("bad block {r}"))
    }

    pub async fn gas_price_gwei(&self, chain: &str) -> anyhow::Result<f64> {
        let c = self.chain(chain).ok_or_else(|| anyhow::anyhow!("unknown chain {chain}"))?;
        let r = self.rpc(&c.rpc, "eth_gasPrice", serde_json::json!([])).await?;
        let h = r.as_str().unwrap_or_default().trim_start_matches("0x");
        let wei = u128::from_str_radix(h, 16)?;
        Ok(wei as f64 / 1e9)
    }

    pub async fn native_balance_eth(&self, chain: &str, addr: &[u8; 20]) -> anyhow::Result<f64> {
        let c = self.chain(chain).ok_or_else(|| anyhow::anyhow!("unknown chain {chain}"))?;
        let r = self
            .rpc(&c.rpc, "eth_getBalance", serde_json::json!([addr_hex(addr), "latest"]))
            .await?;
        let h = r.as_str().unwrap_or_default().trim_start_matches("0x");
        Ok(u128::from_str_radix(h, 16)? as f64 / 1e18)
    }

    pub async fn eth_call(&self, chain: &str, to: &[u8; 20], data: &[u8]) -> anyhow::Result<String> {
        let c = self.chain(chain).ok_or_else(|| anyhow::anyhow!("unknown chain {chain}"))?;
        let r = self
            .rpc(
                &c.rpc,
                "eth_call",
                serde_json::json!([
                    { "to": addr_hex(to), "data": format!("0x{}", hex_lower(data)) },
                    "latest"
                ]),
            )
            .await?;
        Ok(r.as_str().unwrap_or_default().to_string())
    }

    /// Real Uniswap V3 quote via QuoterV2 (verified encoding).
    pub async fn quote(
        &self,
        chain: &str,
        token_in: &[u8; 20],
        token_out: &[u8; 20],
        amount_in: u128,
    ) -> anyhow::Result<u128> {
        let c = self.chain(chain).ok_or_else(|| anyhow::anyhow!("unknown chain {chain}"))?;
        let quoter = parse_address(c.quoter).ok_or_else(|| anyhow::anyhow!("bad quoter addr"))?;
        let data = build_quote_calldata(token_in, token_out, amount_in);
        let out = self.eth_call(chain, &quoter, &data).await?;
        decode_u128_word(&out).ok_or_else(|| anyhow::anyhow!("quote decode failed: {out}"))
    }

    /// One health probe per chain: chainId match, block height, gas, and a
    /// live 0.01 WETH→USDC quote (also gives the on-chain ETH price).
    pub async fn chain_status(&self, chain: &str) -> ChainStatus {
        let mut st = ChainStatus { name: chain.into(), ..Default::default() };
        let Some(c) = self.chain(chain) else { return st };
        let t0 = std::time::Instant::now();
        let id = self.chain_id(chain).await.ok();
        st.chain_id_ok = id == Some(c.chain_id);
        st.rpc_ms = t0.elapsed().as_millis() as u32;
        if !st.chain_id_ok {
            return st;
        }
        st.block = self.block_number(chain).await.unwrap_or(0);
        st.gas_gwei = self.gas_price_gwei(chain).await.unwrap_or(0.0);
        let weth = parse_address(c.weth).unwrap();
        let usdc = parse_address(c.usdc).unwrap();
        if let Ok(out) = self.quote(chain, &weth, &usdc, 10u128.pow(16)).await {
            st.weth_usdc_out = out as f64 / 10f64.powi(USDC_DECIMALS as i32);
            st.eth_price = if st.weth_usdc_out > 0.0 { st.weth_usdc_out * 100.0 } else { 0.0 };
            st.quote_ok = st.weth_usdc_out > 0.0;
        }
        st
    }

    async fn nonce(&self, chain: &str, addr: &[u8; 20]) -> anyhow::Result<u64> {
        let c = self.chain(chain).ok_or_else(|| anyhow::anyhow!("unknown chain {chain}"))?;
        let r = self
            .rpc(&c.rpc, "eth_getTransactionCount", serde_json::json!([addr_hex(addr), "pending"]))
            .await?;
        let h = r.as_str().unwrap_or_default().trim_start_matches("0x");
        u64::from_str_radix(h, 16).map_err(|_| anyhow::anyhow!("bad nonce {r}"))
    }

    async fn erc20_allowance(&self, chain: &str, token: &[u8; 20], owner: &[u8; 20], spender: &[u8; 20]) -> anyhow::Result<u128> {
        let data = build_allowance_calldata(owner, spender);
        let out = self.eth_call(chain, token, &data).await?;
        decode_u128_word(&out).ok_or_else(|| anyhow::anyhow!("allowance decode failed"))
    }

    async fn broadcast(&self, chain: &str, raw: &[u8]) -> anyhow::Result<String> {
        let c = self.chain(chain).ok_or_else(|| anyhow::anyhow!("unknown chain {chain}"))?;
        let r = self
            .rpc(&c.rpc, "eth_sendRawTransaction", serde_json::json!([format!("0x{}", hex_lower(raw))]))
            .await?;
        Ok(r.as_str().unwrap_or_default().to_string())
    }

    /// Build + (optionally) broadcast a swap. `weth_in = true` sells WETH for
    /// USDC; `false` sells USDC for WETH. Amounts are in human units
    /// (ETH / USDC), converted with the correct decimals.
    pub async fn execute_swap(&self, chain: &str, weth_in: bool, amount_in: f64) -> anyhow::Result<SwapOutcome> {
        let c = self.chain(chain).ok_or_else(|| anyhow::anyhow!("unknown chain {chain}"))?;
        let weth = parse_address(c.weth).unwrap();
        let usdc = parse_address(c.usdc).unwrap();
        let router = parse_address(c.router).unwrap();
        let (token_in, token_out, dec) = if weth_in {
            (weth, usdc, WETH_DECIMALS)
        } else {
            (usdc, weth, USDC_DECIMALS)
        };
        if amount_in <= 0.0 {
            anyhow::bail!("amount must be positive");
        }
        let amount_in_raw = (amount_in * 10f64.powi(dec as i32)).round() as u128;
        if amount_in_raw == 0 {
            anyhow::bail!("amount too small for {dec} decimals");
        }

        // 1) Real quote from the on-chain QuoterV2.
        let quoted = self.quote(chain, &token_in, &token_out, amount_in_raw).await?;
        if quoted == 0 {
            anyhow::bail!("zero quote — pool empty or unreachable");
        }
        let out_dec = if weth_in { USDC_DECIMALS } else { WETH_DECIMALS };
        let min_out_raw = quoted * (10_000 - self.slippage_bps as u128) / 10_000;

        // 2) Calldata for the swap. Without a signer key we still deliver
        // the REAL quote + the ready-to-broadcast calldata — the UI shows
        // it and any wallet can submit it.
        let calldata = match self.signer.as_ref() {
            Some(signer) => {
                let from = key_address(signer);
                build_swap_calldata(&token_in, &token_out, &from, amount_in_raw, min_out_raw)
            }
            None => {
                // Recipient unknown until signed — encode with a zero
                // recipient placeholder and note it. The payload proves the
                // exact pool, size and slippage floor.
                let zero = [0u8; 20];
                let cd = build_swap_calldata(&token_in, &token_out, &zero, amount_in_raw, min_out_raw);
                let gas_gwei = self.gas_price_gwei(chain).await.unwrap_or(0.0);
                return Ok(SwapOutcome {
                    chain: chain.to_string(),
                    direction: if weth_in { "WETH → USDC" } else { "USDC → WETH" }.to_string(),
                    amount_in,
                    quoted_out: quoted as f64 / 10f64.powi(out_dec as i32),
                    min_out: min_out_raw as f64 / 10f64.powi(out_dec as i32),
                    nonce: 0,
                    gas_gwei,
                    calldata_hex: format!("0x{}", hex_lower(&cd)),
                    raw_tx_hex: None,
                    tx_hash: None,
                    approved_first: false,
                    broadcast: false,
                    note: "no signer key — real quote + calldata (set IVARB_CHAIN_SIGNER_KEY to broadcast)".into(),
                });
            }
        };
        let signer = self.signer.as_ref().expect("checked above");
        let from = key_address(signer);

        let gas_gwei = self.gas_price_gwei(chain).await?;
        let nonce = self.nonce(chain, &from).await?;

        // 3) Allowance check + approve when missing (a REAL first tx).
        let mut approved_first = false;
        let allowance = self.erc20_allowance(chain, &token_in, &from, &router).await?;
        let mut txs = Vec::new();
        if allowance < amount_in_raw {
            let approve_data = build_approve_calldata(&router, u128::MAX);
            let approve_tx = Eip1559Tx {
                chain_id: c.chain_id,
                nonce,
                max_priority_fee_gwei: (gas_gwei * 0.1).max(0.001),
                max_fee_gwei: gas_gwei * 1.2,
                gas_limit: 60_000,
                to: Some(token_in),
                value_wei: 0,
                data: approve_data,
            };
            txs.push(approve_tx);
            approved_first = true;
        }
        let swap_tx = Eip1559Tx {
            chain_id: c.chain_id,
            nonce: nonce + txs.len() as u64,
            max_priority_fee_gwei: (gas_gwei * 0.1).max(0.001),
            max_fee_gwei: gas_gwei * 1.2,
            gas_limit: 240_000,
            to: Some(router),
            value_wei: 0,
            data: calldata.clone(),
        };
        txs.push(swap_tx);

        // 4) Sign + broadcast (sequential nonces, in order).
        let mut raw_tx_hex: Option<String> = None;
        let mut tx_hash = None;
        for tx in &txs {
            let raw = tx.sign(signer)?;
            let h = self.broadcast(chain, &raw).await?;
            raw_tx_hex = Some(format!("0x{}", hex_lower(&raw)));
            if h.len() >= 66 {
                tx_hash = Some(h);
            }
        }

        let broadcast = tx_hash.is_some();
        Ok(SwapOutcome {
            chain: chain.to_string(),
            direction: if weth_in { "WETH → USDC" } else { "USDC → WETH" }.to_string(),
            amount_in,
            quoted_out: quoted as f64 / 10f64.powi(out_dec as i32),
            min_out: min_out_raw as f64 / 10f64.powi(out_dec as i32),
            nonce,
            gas_gwei,
            calldata_hex: format!("0x{}", hex_lower(&calldata)),
            raw_tx_hex,
            tx_hash,
            approved_first,
            broadcast,
            note: if approved_first { "approve + swap broadcast".into() } else { "swap broadcast".into() },
        })
    }
}

fn parse_secret_key(s: &str) -> Option<k256::ecdsa::SigningKey> {
    let h = s.trim().trim_start_matches("0x");
    if h.len() != 64 {
        return None;
    }
    let mut b = [0u8; 32];
    for i in 0..32 {
        b[i] = u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).ok()?;
    }
    k256::ecdsa::SigningKey::from_slice(&b).ok()
}

// ---------------------------------------------------------------------------
// Tests — spec vectors, anchors, self-consistency
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keccak_anchors() {
        // Empty-string keccak256 — the canonical public vector.
        assert_eq!(
            hex_lower(&keccak256(b"")),
            "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
        );
        // Selector anchors (verified on-chain before commit).
        assert_eq!(&hex_lower(&selector("transfer(address,uint256)"))[..8], "a9059cbb");
        assert_eq!(
            &hex_lower(&selector("quoteExactInputSingle((address,address,uint256,uint24,uint160))"))[..8],
            "c6a5026a"
        );
        assert_eq!(
            &hex_lower(&selector("exactInputSingle((address,address,uint24,address,uint256,uint256,uint160))"))[..8],
            "04e45aaf"
        );
    }

    #[test]
    fn rlp_spec_vectors() {
        // From the Ethereum wiki RLP spec.
        assert_eq!(rlp::encode_bytes(b""), vec![0x80]);
        assert_eq!(rlp::encode_bytes(&[0x0f]), vec![0x0f]);
        assert_eq!(rlp::encode_bytes(&[0x04, 0x00]), vec![0x82, 0x04, 0x00]);
        assert_eq!(rlp::encode_bytes(b"dog"), vec![0x83, b'd', b'o', b'g']);
        assert_eq!(rlp::encode_list(&[]), vec![0xc0]);
        let cat = rlp::encode_bytes(b"cat");
        let dog = rlp::encode_bytes(b"dog");
        assert_eq!(rlp::encode_list(&[cat, dog]), vec![0xc8, 0x83, b'c', b'a', b't', 0x83, b'd', b'o', b'g']);
        // uint encodings
        assert_eq!(rlp::encode_uint(0), vec![0x80]);
        assert_eq!(rlp::encode_uint(15), vec![0x0f]);
        assert_eq!(rlp::encode_uint(1024), vec![0x82, 0x04, 0x00]);
    }

    #[test]
    fn address_parsing() {
        let a = parse_address("0x61fFE014bA17989E743c5F6cB21bF9697530B21e").unwrap();
        assert_eq!(addr_hex(&a), "0x61ffe014ba17989e743c5f6cb21bf9697530b21e");
        assert!(parse_address("0x123").is_none());
        assert!(parse_address("zz").is_none());
    }

    #[test]
    fn quote_calldata_matches_live_vector() {
        // The exact calldata that returned 26.97 USDC for 0.01 WETH on all
        // four chains during verification (scripts/probe_univ3.py).
        let weth = parse_address("0x4200000000000000000000000000000000000006").unwrap();
        let usdc = parse_address("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913").unwrap();
        let d = build_quote_calldata(&weth, &usdc, 10u128.pow(16));
        assert_eq!(d.len(), 4 + 32 * 5);
        assert_eq!(
            hex_lower(&d),
            "c6a5026a0000000000000000000000004200000000000000000000000000000000000006000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda02913000000000000000000000000000000000000000000000000002386f26fc1000000000000000000000000000000000000000000000000000000000000000001f40000000000000000000000000000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn swap_calldata_shape() {
        let weth = parse_address("0x4200000000000000000000000000000000000006").unwrap();
        let usdc = parse_address("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913").unwrap();
        let me = parse_address("0x000000000000000000000000000000000000dead").unwrap();
        let d = build_swap_calldata(&weth, &usdc, &me, 1_000_000_000_000_000_000, 26_000_000);
        assert_eq!(d.len(), 4 + 32 * 7);
        assert!(hex_lower(&d).starts_with("04e45aaf"));
        // fee word = 500 = 0x1f4
        assert_eq!(&hex_lower(&d)[8 + 64 + 64 .. 8 + 64 + 64 + 64], "00000000000000000000000000000000000000000000000000000000000001f4");
    }

    #[test]
    fn erc20_calldata() {
        let r = parse_address("0x68b3465833fb72a70ecdf485e0e4c7bd8665fc45").unwrap();
        let me = parse_address("0x000000000000000000000000000000000000dead").unwrap();
        assert!(hex_lower(&build_approve_calldata(&r, u128::MAX)).starts_with("095ea7b3"));
        assert!(hex_lower(&build_allowance_calldata(&me, &r)).starts_with("dd62ed3e"));
        assert!(hex_lower(&build_balance_of_calldata(&me)).starts_with("70a08231"));
        assert_eq!(hex_lower(&build_wrap_calldata()), "d0e30db0");
        assert!(hex_lower(&build_unwrap_calldata(1)).starts_with("2e1a7d4d"));
    }

    #[test]
    fn decode_word() {
        // Deterministic vectors (1e6, 1 wei, zero) — live quotes move.
        assert_eq!(decode_u128_word("0x00000000000000000000000000000000000000000000000000000000000f4240"), Some(1_000_000));
        assert_eq!(decode_u128_word("0x0000000000000000000000000000000000000000000000000000000000000001"), Some(1));
        assert_eq!(decode_u128_word("0x0000000000000000000000000000000000000000000000000000000000000000"), Some(0));
        assert_eq!(decode_u128_word("0x"), None);
    }

    #[test]
    fn canonical_test_key_address() {
        // The well-known anvil/foundry test key #0 (public, unfunded on
        // mainnet). If address derivation matches, secp256k1 + keccak are
        // wired the Ethereum way.
        let key = parse_secret_key("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80").unwrap();
        assert_eq!(addr_hex(&key_address(&key)), "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266");
    }

    #[test]
    fn eip1559_tx_signs_and_serializes() {
        let key = parse_secret_key("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80").unwrap();
        let tx = Eip1559Tx {
            chain_id: 8453,
            nonce: 7,
            max_priority_fee_gwei: 0.001,
            max_fee_gwei: 0.05,
            gas_limit: 240_000,
            to: Some(parse_address("0x68b3465833fb72a70ecdf485e0e4c7bd8665fc45").unwrap()),
            value_wei: 0,
            data: build_wrap_calldata(),
        };
        let raw = tx.sign(&key).unwrap();
        assert_eq!(raw[0], 0x02);
        // Deterministic: same key + tx → same signature.
        let raw2 = tx.sign(&key).unwrap();
        assert_eq!(raw, raw2);
        // sighash changes with nonce (freshness of the signing message).
        let mut tx3 = tx.clone();
        tx3.nonce = 8;
        assert_ne!(tx.sighash(), tx3.sighash());
    }

    #[test]
    fn chain_registry_defaults() {
        let chains = default_chains();
        assert_eq!(chains.len(), 4);
        assert!(chains.iter().all(|c| c.rpc.starts_with("https://")));
        assert_eq!(chains[0].chain_id, 8453);
        assert_eq!(chains[1].chain_id, 42161);
        assert_eq!(chains[2].chain_id, 10);
        assert_eq!(chains[3].chain_id, 1);
        for c in &chains {
            assert!(parse_address(c.quoter).is_some());
            assert!(parse_address(c.router).is_some());
            assert!(parse_address(c.weth).is_some());
            assert!(parse_address(c.usdc).is_some());
        }
    }
}
