// Meowcoin types — use Bitcoin crate for all transaction/script types (fully compatible),
// but use custom header/block types to handle the variable-size Meowcoin header formats.

pub use bitcoin::{
    address, blockdata::script, consensus::deserialize, hash_types::TxMerkleNode, Address,
    BlockHash, OutPoint, ScriptBuf as Script, Sequence, Transaction, TxIn, TxOut, Txid,
};

pub use crate::meowcoin::{MeowcoinBlock as Block, MeowcoinHeader as BlockHeader};
pub type Value = u64;

// ─── Network enum ────────────────────────────────────────────────────────────

#[derive(Debug, Copy, Clone, PartialEq, Hash, Serialize, Ord, PartialOrd, Eq)]
pub enum Network {
    /// Meowcoin mainnet
    Mainnet,
    /// Meowcoin testnet
    Testnet,
    /// Meowcoin signet
    Signet,
    /// Meowcoin regtest
    Regtest,
}

impl Network {
    /// Network magic as u32 (little-endian bytes in blk*.dat files).
    pub fn magic(self) -> u32 {
        match self {
            // "MEWC" bytes: 0x4D 0x45 0x57 0x43 → LE u32
            Network::Mainnet => u32::from_le_bytes([0x4D, 0x45, 0x57, 0x43]),
            // "nfxd" bytes: 0x6e 0x66 0x78 0x64
            Network::Testnet => u32::from_le_bytes([0x6e, 0x66, 0x78, 0x64]),
            // "SIGN" bytes: 0x53 0x49 0x47 0x4e
            Network::Signet => u32::from_le_bytes([0x53, 0x49, 0x47, 0x4e]),
            // "DROW" bytes: 0x44 0x52 0x4F 0x57
            Network::Regtest => u32::from_le_bytes([0x44, 0x52, 0x4F, 0x57]),
        }
    }

    pub fn is_regtest(self) -> bool {
        self == Network::Regtest
    }

    /// Base58 version byte for P2PKH addresses.
    pub fn p2pkh_prefix(self) -> u8 {
        match self {
            Network::Mainnet => 50,  // 0x32 → addresses start with "M"
            Network::Testnet | Network::Signet => 109, // 0x6d → "m"
            Network::Regtest => 42,  // 0x2a
        }
    }

    /// Base58 version byte for P2SH addresses.
    pub fn p2sh_prefix(self) -> u8 {
        match self {
            Network::Mainnet => 122, // 0x7A
            Network::Testnet | Network::Signet | Network::Regtest => 124, // 0x7C
        }
    }

    /// Bech32 human-readable part for SegWit addresses.
    pub fn bech32_hrp(self) -> &'static str {
        match self {
            Network::Mainnet => "mewc",
            Network::Testnet => "tmewc",
            Network::Signet => "smewc",
            Network::Regtest => "rmewc",
        }
    }

    pub fn names() -> Vec<String> {
        vec![
            "mainnet".to_string(),
            "testnet".to_string(),
            "signet".to_string(),
            "regtest".to_string(),
        ]
    }
}

// ─── Genesis hashes ──────────────────────────────────────────────────────────

pub fn genesis_hash(network: Network) -> BlockHash {
    lazy_static! {
        static ref MAINNET_GENESIS: BlockHash =
            "000000edd819220359469c54f2614b5602ebc775ea67a64602f354bdaa320f70"
                .parse()
                .unwrap();
        static ref TESTNET_GENESIS: BlockHash =
            "000000eaab417d6dfe9bd75119972e1d07ecfe8ff655bef7c2acb3d9a0eeed81"
                .parse()
                .unwrap();
        static ref REGTEST_GENESIS: BlockHash =
            "530827f38f93b43ed12af0b3ad25a288dc02ed74d6d7857862df51fc56c416f9"
                .parse()
                .unwrap();
    }
    match network {
        Network::Mainnet => *MAINNET_GENESIS,
        Network::Testnet => *TESTNET_GENESIS,
        // Signet and regtest share the regtest genesis for electrum-discovery purposes.
        Network::Signet | Network::Regtest => *REGTEST_GENESIS,
    }
}

// ─── Network name parsing ────────────────────────────────────────────────────

impl From<&str> for Network {
    fn from(network_name: &str) -> Self {
        match network_name {
            "mainnet" => Network::Mainnet,
            "testnet" => Network::Testnet,
            "signet" => Network::Signet,
            "regtest" => Network::Regtest,
            _ => panic!("unsupported Meowcoin network: {:?}", network_name),
        }
    }
}
