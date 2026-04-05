use crate::chain::{script, Network, Script, TxIn, TxOut};
use script::Instruction::PushBytes;

use bitcoin::hashes::{sha256d, Hash};

pub struct InnerScripts {
    pub redeem_script: Option<Script>,
    pub witness_script: Option<Script>,
}

pub trait ScriptToAsm: std::fmt::Debug {
    fn to_asm(&self) -> String {
        let asm = format!("{:?}", self);
        (&asm[7..asm.len() - 1]).to_string()
    }
}
impl ScriptToAsm for bitcoin::ScriptBuf {}

pub trait ScriptToAddr {
    fn to_address_str(&self, network: Network) -> Option<String>;
}

impl ScriptToAddr for bitcoin::Script {
    fn to_address_str(&self, network: Network) -> Option<String> {
        meowcoin_script_to_address(self, network)
    }
}

/// Decode a Meowcoin address string into the corresponding output script.
///
/// Supports P2PKH, P2SH (base58check with Meowcoin version bytes) and
/// all SegWit address types (bech32/bech32m with Meowcoin HRP).
pub fn meowcoin_address_to_script(addr: &str, network: Network) -> Option<bitcoin::ScriptBuf> {
    use bitcoin::hashes::{hash160, Hash};

    // Try SegWit (bech32 / bech32m) first.
    let expected_hrp = bech32::Hrp::parse(network.bech32_hrp()).ok()?;
    if let Ok((got_hrp, version, program)) = bech32::segwit::decode(addr) {
        if got_hrp == expected_hrp {
            return build_witness_script(version, &program);
        }
    }

    // Try base58check (P2PKH / P2SH).
    if let Ok(payload) = bitcoin::base58::decode_check(addr) {
        if payload.len() == 21 {
            let prefix = payload[0];
            let hash_bytes = &payload[1..21];
            let h160 = hash160::Hash::from_slice(hash_bytes).ok()?;
            if prefix == network.p2pkh_prefix() {
                return Some(bitcoin::ScriptBuf::new_p2pkh(
                    &bitcoin::PubkeyHash::from_raw_hash(h160),
                ));
            } else if prefix == network.p2sh_prefix() {
                return Some(bitcoin::ScriptBuf::new_p2sh(
                    &bitcoin::ScriptHash::from_raw_hash(h160),
                ));
            }
        }
    }

    None
}

/// Build a SegWit scriptPubKey from a witness version and program bytes.
/// Layout: [version_opcode, push_n, ...program]  (program ≤ 40 bytes)
fn build_witness_script(version: bech32::Fe32, program: &[u8]) -> Option<bitcoin::ScriptBuf> {
    let v = version.to_u8();
    if v > 16 || program.is_empty() || program.len() > 40 {
        return None;
    }
    // OP_0 = 0x00; OP_1..OP_16 = 0x51..0x60
    let version_opcode = if v == 0 { 0x00_u8 } else { 0x50 + v };
    let mut script = Vec::with_capacity(2 + program.len());
    script.push(version_opcode);
    script.push(program.len() as u8); // direct push for ≤40 bytes (no PUSHDATA needed)
    script.extend_from_slice(program);
    Some(bitcoin::ScriptBuf::from(script))
}

/// Encode a script as a Meowcoin address string using Meowcoin's custom address parameters.
fn meowcoin_script_to_address(script: &bitcoin::Script, network: Network) -> Option<String> {
    let bytes = script.as_bytes();

    // P2PKH: OP_DUP OP_HASH160 <20 bytes> OP_EQUALVERIFY OP_CHECKSIG (25 bytes)
    if script.is_p2pkh() && bytes.len() == 25 {
        let hash20 = &bytes[3..23];
        return Some(base58check_encode(network.p2pkh_prefix(), hash20));
    }

    // P2SH: OP_HASH160 <20 bytes> OP_EQUAL (23 bytes)
    if script.is_p2sh() && bytes.len() == 23 {
        let hash20 = &bytes[2..22];
        return Some(base58check_encode(network.p2sh_prefix(), hash20));
    }

    // SegWit addresses (bech32 / bech32m)
    let hrp_str = network.bech32_hrp();
    let hrp = bech32::Hrp::parse(hrp_str).ok()?;

    // P2WPKH: OP_0 <20 bytes> (22 bytes)
    if script.is_p2wpkh() && bytes.len() == 22 {
        let program = &bytes[2..22];
        return bech32::segwit::encode(hrp, bech32::Fe32::Q, program).ok();
    }

    // P2WSH: OP_0 <32 bytes> (34 bytes)
    if script.is_p2wsh() && bytes.len() == 34 {
        let program = &bytes[2..34];
        return bech32::segwit::encode(hrp, bech32::Fe32::Q, program).ok();
    }

    // P2TR: OP_1 <32 bytes> (34 bytes)
    if script.is_p2tr() && bytes.len() == 34 {
        let program = &bytes[2..34];
        return bech32::segwit::encode(hrp, bech32::Fe32::P, program).ok();
    }

    None
}

/// Base58Check encode a payload with the given version byte.
/// Appends the first 4 bytes of SHA256d(version || payload) as checksum.
fn base58check_encode(version: u8, payload: &[u8]) -> String {
    let mut data = Vec::with_capacity(1 + payload.len() + 4);
    data.push(version);
    data.extend_from_slice(payload);
    let checksum = sha256d::Hash::hash(&data);
    data.extend_from_slice(&checksum.as_byte_array()[..4]);
    bitcoin::base58::encode(&data)
}

// Returns the witnessScript in the case of p2wsh, or the redeemScript in the case of p2sh.
pub fn get_innerscripts(txin: &TxIn, prevout: &TxOut) -> InnerScripts {
    // Wrapped redeemScript for P2SH spends
    let redeem_script = if prevout.script_pubkey.is_p2sh() {
        if let Some(Ok(PushBytes(redeemscript))) = txin.script_sig.instructions().last() {
            #[cfg(not(feature = "liquid"))] // rust-bitcoin has a PushBytes wrapper type
            let redeemscript = redeemscript.as_bytes();
            Some(Script::from(redeemscript.to_vec()))
        } else {
            None
        }
    } else {
        None
    };

    // Wrapped witnessScript for P2WSH or P2SH-P2WSH spends
    let witness_script = if prevout.script_pubkey.is_p2wsh()
        || redeem_script.as_ref().map_or(false, |s| s.is_p2wsh())
    {
        let witness = &txin.witness;
        #[cfg(feature = "liquid")]
        let witness = &witness.script_witness;

        // rust-bitcoin returns witness items as a [u8] slice, while rust-elements returns a Vec<u8>
        #[cfg(not(feature = "liquid"))]
        let wit_to_vec = Vec::from;
        #[cfg(feature = "liquid")]
        let wit_to_vec = Clone::clone;

        witness.iter().last().map(wit_to_vec).map(Script::from)
    } else {
        None
    };

    InnerScripts {
        redeem_script,
        witness_script,
    }
}
