//! Meowcoin-specific blockchain data types and block deserialization.
//!
//! Meowcoin has three block header formats:
//!
//! 1. **Pre-KAWPOW** (`nTime < KAWPOW_ACTIVATION_TIME`, not AuxPoW): standard 80-byte header
//!    (nVersion + hashPrevBlock + hashMerkleRoot + nTime + nBits + nNonce)
//!
//! 2. **KAWPOW/MEOWPOW** (`nTime >= KAWPOW_ACTIVATION_TIME`, not AuxPoW): 120-byte header
//!    (76-byte base + nHeight(4) + nNonce64(8) + mix_hash(32))
//!
//! 3. **AuxPoW** (VERSION_AUXPOW bit set, any time): 80-byte header + variable AuxPoW data
//!    (always uses nNonce regardless of timestamp)
//!
//! Since electrs doesn't verify PoW, extra fields are parsed and discarded.
//! Block hashes MUST be provided externally (from RPC) because KAWPOW/MEOWPOW
//! uses a different hash function than SHA256d.

use std::io::{Cursor, Read};

use bitcoin::hashes::Hash;
use bitcoin::consensus::encode::{Decodable, VarInt};
use bitcoin::{BlockHash, TxMerkleNode, Transaction, Weight};

use crate::errors::*;

/// KAWPOW/MEOWPOW activation timestamp.
/// Blocks with `nTime >= KAWPOW_ACTIVATION_TIME` (and no AuxPoW) use 120-byte headers.
pub const KAWPOW_ACTIVATION_TIME: u32 = 1_662_493_424;

/// AuxPoW flag in block version field (bit 8).
pub const VERSION_AUXPOW: u32 = 1 << 8;

/// A Meowcoin block header with its hash stored from the RPC.
///
/// The actual Meowcoin block hash (MEOWPOW, Scrypt-AuxPoW, or SHA256d) cannot be
/// recomputed from the raw bytes without the full MEOWPOW/KAWPOW implementation,
/// so the hash is always provided externally by the node RPC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeowcoinHeader {
    /// Block version (includes AuxPoW bit and chain ID in bits 16-19).
    pub version: i32,
    /// Hash of the previous block.
    pub prev_blockhash: BlockHash,
    /// Merkle root of block transactions.
    pub merkle_root: TxMerkleNode,
    /// Block timestamp.
    pub time: u32,
    /// Compact difficulty target.
    pub bits: u32,
    /// nNonce from the raw header (0 for KAWPOW/MEOWPOW blocks since we discard nNonce64).
    pub nonce: u32,
    /// The actual Meowcoin block hash, provided by the node RPC.
    hash: BlockHash,
    /// Raw header bytes (80 or 120 bytes) for Electrum protocol serialization.
    raw_bytes: Vec<u8>,
}

impl MeowcoinHeader {
    /// Returns the block hash (as provided by the Meowcoin node RPC).
    pub fn block_hash(&self) -> BlockHash {
        self.hash
    }

    /// Raw header bytes for Electrum protocol serialization.
    pub fn raw_bytes(&self) -> &[u8] {
        &self.raw_bytes
    }

    /// Approximate block difficulty (ratio of max target to current target).
    /// Uses the Bitcoin genesis max target as reference.
    pub fn difficulty_float(&self) -> f64 {
        // max_bits = 0x1d00ffff (Bitcoin genesis difficulty target)
        // difficulty = max_target / current_target
        let max_bits = 0x1d00ffff_u32;
        let max_exp = (max_bits >> 24) as i32;
        let max_mantissa = (max_bits & 0x00ff_ffff) as f64;

        let cur_exp = (self.bits >> 24) as i32;
        let cur_mantissa = (self.bits & 0x00ff_ffff) as f64;

        if cur_mantissa == 0.0 {
            return 0.0;
        }

        let exponent = (max_exp - cur_exp) * 8;
        let mantissa_ratio = max_mantissa / cur_mantissa;

        if exponent >= 0 {
            mantissa_ratio * (1u64 << exponent.min(63)) as f64
        } else {
            mantissa_ratio / (1u64 << (-exponent).min(63)) as f64
        }
    }
}

/// Allow `serialize_hex(header)` in the Electrum server — writes the raw header bytes.
impl bitcoin::consensus::encode::Encodable for MeowcoinHeader {
    fn consensus_encode<W: bitcoin::io::Write + ?Sized>(
        &self,
        w: &mut W,
    ) -> std::result::Result<usize, bitcoin::io::Error> {
        w.write_all(&self.raw_bytes)?;
        Ok(self.raw_bytes.len())
    }
}

/// A Meowcoin block consisting of a header and Bitcoin-compatible transactions.
/// Meowcoin transactions are fully compatible with Bitcoin (segwit-enabled).
#[derive(Debug, Clone)]
pub struct MeowcoinBlock {
    pub header: MeowcoinHeader,
    pub txdata: Vec<Transaction>,
}

impl MeowcoinBlock {
    /// Returns the block hash (as provided by the Meowcoin node RPC).
    pub fn block_hash(&self) -> BlockHash {
        self.header.block_hash()
    }

    /// Total serialized byte size (header + varint(tx_count) + transactions).
    pub fn total_size(&self) -> usize {
        let header_size = self.header.raw_bytes.len();
        let varint_size = VarInt(self.txdata.len() as u64).size();
        let txs_size: usize = self
            .txdata
            .iter()
            .map(|tx| bitcoin::consensus::encode::serialize(tx).len())
            .sum();
        header_size + varint_size + txs_size
    }

    /// Sum of transaction weights.
    pub fn weight(&self) -> Weight {
        self.txdata
            .iter()
            .fold(Weight::ZERO, |acc, tx| acc + tx.weight())
    }
}

/// Parse a Meowcoin block from raw RPC bytes, using the known block hash.
///
/// Handles all three header formats:
/// - Pre-KAWPOW (80 bytes)
/// - KAWPOW/MEOWPOW (120 bytes)
/// - AuxPoW (80-byte header + variable AuxPoW data)
pub fn deserialize_block(hash: BlockHash, raw: &[u8]) -> Result<MeowcoinBlock> {
    let mut cursor = Cursor::new(raw);

    // Read the common 76-byte base
    let version = read_u32(&mut cursor).chain_err(|| "block version")?;
    let prev_blockhash = read_block_hash(&mut cursor).chain_err(|| "block prev_blockhash")?;
    let merkle_root = read_tx_merkle_node(&mut cursor).chain_err(|| "block merkle_root")?;
    let time = read_u32(&mut cursor).chain_err(|| "block time")?;
    let bits = read_u32(&mut cursor).chain_err(|| "block bits")?;

    let is_auxpow = (version & VERSION_AUXPOW) != 0;

    let (raw_bytes, nonce) = if is_auxpow || time < KAWPOW_ACTIVATION_TIME {
        // 80-byte header: base(76) + nNonce(4)
        let nonce = read_u32(&mut cursor).chain_err(|| "block nNonce")?;
        let raw_bytes = raw[..80].to_vec();
        if is_auxpow {
            skip_auxpow_data(&mut cursor)
                .chain_err(|| "failed to skip AuxPoW data")?;
        }
        (raw_bytes, nonce)
    } else {
        // 120-byte header: base(76) + nHeight(4) + nNonce64(8) + mix_hash(32)
        let _height = read_u32(&mut cursor).chain_err(|| "block nHeight")?;
        let _nonce64 = read_u64(&mut cursor).chain_err(|| "block nNonce64")?;
        skip_exact::<32>(&mut cursor).chain_err(|| "block mix_hash")?;
        let raw_bytes = raw[..120].to_vec();
        (raw_bytes, 0u32)
    };

    let txdata = decode_transactions(&mut cursor)?;

    Ok(MeowcoinBlock {
        header: MeowcoinHeader {
            version: version as i32,
            prev_blockhash,
            merkle_root,
            time,
            bits,
            nonce,
            hash,
            raw_bytes,
        },
        txdata,
    })
}

/// Parse only the header from raw header bytes (as returned by `getblockheader verbose=false`).
pub fn deserialize_header(hash: BlockHash, raw: &[u8]) -> Result<MeowcoinHeader> {
    ensure!(raw.len() >= 80, "header too short: {} bytes", raw.len());

    let mut cursor = Cursor::new(raw);
    let version = read_u32(&mut cursor).chain_err(|| "header version")?;
    let prev_blockhash = read_block_hash(&mut cursor).chain_err(|| "header prev_blockhash")?;
    let merkle_root = read_tx_merkle_node(&mut cursor).chain_err(|| "header merkle_root")?;
    let time = read_u32(&mut cursor).chain_err(|| "header time")?;
    let bits = read_u32(&mut cursor).chain_err(|| "header bits")?;

    let is_auxpow = (version & VERSION_AUXPOW) != 0;

    let (header_len, nonce) = if !is_auxpow && time >= KAWPOW_ACTIVATION_TIME && raw.len() >= 120 {
        (120usize, 0u32)
    } else {
        let nonce = read_u32(&mut cursor).chain_err(|| "header nNonce")?;
        (80usize, nonce)
    };

    Ok(MeowcoinHeader {
        version: version as i32,
        prev_blockhash,
        merkle_root,
        time,
        bits,
        nonce,
        hash,
        raw_bytes: raw[..header_len].to_vec(),
    })
}

/// Skip AuxPoW data in the stream.
///
/// AuxPoW structure (CAuxPow inherits CMerkleTx):
/// - CMerkleTx: parent coinbase CTransaction + hashBlock(32) + vMerkleBranch + nIndex(4)
/// - vChainMerkleBranch: VarInt + n×32 bytes
/// - nChainIndex: 4 bytes
/// - parentBlock: CPureBlockHeader (80 bytes = version + prev + merkle + time + bits + nonce)
fn skip_auxpow_data(cursor: &mut Cursor<&[u8]>) -> Result<()> {
    // Parent coinbase transaction (standard Bitcoin transaction)
    let _coinbase_tx = Transaction::consensus_decode(cursor)
        .chain_err(|| "AuxPoW coinbase transaction")?;

    // hashBlock (32 bytes)
    skip_exact::<32>(cursor).chain_err(|| "AuxPoW hashBlock")?;

    // vMerkleBranch: VarInt count + count×32-byte hashes
    skip_hash_vec(cursor).chain_err(|| "AuxPoW vMerkleBranch")?;

    // nIndex (4 bytes)
    read_u32(cursor).chain_err(|| "AuxPoW nIndex")?;

    // vChainMerkleBranch: VarInt count + count×32-byte hashes
    skip_hash_vec(cursor).chain_err(|| "AuxPoW vChainMerkleBranch")?;

    // nChainIndex (4 bytes)
    read_u32(cursor).chain_err(|| "AuxPoW nChainIndex")?;

    // parentBlock = CPureBlockHeader (80 bytes)
    read_u32(cursor).chain_err(|| "parentBlock nVersion")?;
    skip_exact::<32>(cursor).chain_err(|| "parentBlock hashPrevBlock")?;
    skip_exact::<32>(cursor).chain_err(|| "parentBlock hashMerkleRoot")?;
    read_u32(cursor).chain_err(|| "parentBlock nTime")?;
    read_u32(cursor).chain_err(|| "parentBlock nBits")?;
    read_u32(cursor).chain_err(|| "parentBlock nNonce")?;

    Ok(())
}

/// Skip a vector of 32-byte hashes (VarInt count + count×32 bytes).
fn skip_hash_vec(cursor: &mut Cursor<&[u8]>) -> Result<()> {
    let count = VarInt::consensus_decode(cursor)
        .chain_err(|| "hash vec count")?
        .0;
    for _ in 0..count {
        skip_exact::<32>(cursor).chain_err(|| "hash vec entry")?;
    }
    Ok(())
}

/// Parse the transaction list (VarInt count + transactions).
fn decode_transactions(cursor: &mut Cursor<&[u8]>) -> Result<Vec<Transaction>> {
    let count = VarInt::consensus_decode(cursor)
        .chain_err(|| "transaction count")?
        .0 as usize;
    let mut txdata = Vec::with_capacity(count);
    for i in 0..count {
        let tx = Transaction::consensus_decode(cursor)
            .chain_err(|| format!("transaction #{}", i))?;
        txdata.push(tx);
    }
    Ok(txdata)
}

// ─── Low-level read helpers ──────────────────────────────────────────────────

fn read_u32(r: &mut impl Read) -> Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf).chain_err(|| "read u32")?;
    Ok(u32::from_le_bytes(buf))
}

fn read_u64(r: &mut impl Read) -> Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf).chain_err(|| "read u64")?;
    Ok(u64::from_le_bytes(buf))
}

fn read_block_hash(r: &mut impl Read) -> Result<BlockHash> {
    let mut buf = [0u8; 32];
    r.read_exact(&mut buf).chain_err(|| "read BlockHash")?;
    Ok(BlockHash::from_byte_array(buf))
}

fn read_tx_merkle_node(r: &mut impl Read) -> Result<TxMerkleNode> {
    let mut buf = [0u8; 32];
    r.read_exact(&mut buf).chain_err(|| "read TxMerkleNode")?;
    Ok(TxMerkleNode::from_byte_array(buf))
}

fn skip_exact<const N: usize>(r: &mut impl Read) -> Result<()> {
    let mut buf = [0u8; N];
    r.read_exact(&mut buf).chain_err(|| format!("skip {} bytes", N))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hex::FromHex;

    /// Fetch a raw block or header from the running Meowcoin node via curl.
    #[cfg(test)]
    fn rpc_getraw(method: &str, params: &str) -> String {
        let body = format!(
            r#"{{"jsonrpc":"1.0","id":"t","method":"{}","params":{}}}"#,
            method, params
        );
        let out = std::process::Command::new("curl")
            .args([
                "-s", "--user", "meow:supersecurepassword",
                "--data-binary", &body,
                "-H", "content-type:text/plain;",
                "http://127.0.0.1:18332/",
            ])
            .output()
            .expect("curl")
            .stdout;
        let v: serde_json::Value = serde_json::from_slice(&out).expect("json");
        v["result"].as_str().unwrap().to_string()
    }

    fn rpc_hash(method: &str, params: &str) -> BlockHash {
        rpc_getraw(method, params).parse().unwrap()
    }

    // ── Pre-KAWPOW block (block 1, time=1662492762 < 1662493424) ──────────────
    #[test]
    fn test_parse_prekawpow_header() {
        let hash = rpc_hash("getblockhash", "[1]");
        let raw_hex = rpc_getraw("getblockheader", &format!("[\"{}\",false]", hash));
        let raw = Vec::<u8>::from_hex(&raw_hex).unwrap();
        assert_eq!(raw.len(), 80, "pre-KAWPOW header must be 80 bytes");
        let hdr = deserialize_header(hash, &raw).expect("deserialize_header");
        assert_eq!(hdr.block_hash(), hash);
        assert_eq!(hdr.raw_bytes().len(), 80);
        assert!(hdr.time < KAWPOW_ACTIVATION_TIME, "block 1 must be pre-KAWPOW");
    }

    // ── KAWPOW/MEOWPOW block (block 1000, time=1662532418 > 1662493424) ───────
    #[test]
    fn test_parse_kawpow_header() {
        let hash = rpc_hash("getblockhash", "[1000]");
        let raw_hex = rpc_getraw("getblockheader", &format!("[\"{}\",false]", hash));
        let raw = Vec::<u8>::from_hex(&raw_hex).unwrap();
        assert_eq!(raw.len(), 120, "KAWPOW header must be 120 bytes");
        let hdr = deserialize_header(hash, &raw).expect("deserialize_header");
        assert_eq!(hdr.block_hash(), hash);
        assert_eq!(hdr.raw_bytes().len(), 120);
        assert!(hdr.time >= KAWPOW_ACTIVATION_TIME);
    }

    // ── AuxPoW block (near tip, version & 0x100) ──────────────────────────────
    #[test]
    fn test_parse_auxpow_block() {
        // Block 1860764 is a known AuxPoW block (version=0x30090100)
        let hash = rpc_hash("getblockhash", "[1860764]");
        let raw_hex = rpc_getraw("getblock", &format!("[\"{}\",false]", hash));
        let raw = Vec::<u8>::from_hex(&raw_hex).unwrap();
        let block = deserialize_block(hash, &raw).expect("deserialize_block auxpow");
        assert_eq!(block.block_hash(), hash);
        assert!(!block.txdata.is_empty(), "AuxPoW block must have transactions");
        // AuxPoW base header is 80 bytes
        assert_eq!(block.header.raw_bytes().len(), 80);
        assert!((block.header.version as u32) & VERSION_AUXPOW != 0);
    }

    // ── MEOWPOW native block near tip ─────────────────────────────────────────
    #[test]
    fn test_parse_meowpow_block() {
        // Block 1860767 is a known native MEOWPOW block (version=0x30090000)
        let hash = rpc_hash("getblockhash", "[1860767]");
        let raw_hex = rpc_getraw("getblock", &format!("[\"{}\",false]", hash));
        let raw = Vec::<u8>::from_hex(&raw_hex).unwrap();
        let block = deserialize_block(hash, &raw).expect("deserialize_block meowpow");
        assert_eq!(block.block_hash(), hash);
        assert!(!block.txdata.is_empty());
        assert_eq!(block.header.raw_bytes().len(), 120);
        assert_eq!((block.header.version as u32) & VERSION_AUXPOW, 0);
    }
}
