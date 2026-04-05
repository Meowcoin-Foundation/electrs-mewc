# Esplora - Electrs backend API (Meowcoin fork)

A block chain index engine and HTTP API written in Rust based on [romanz/electrs](https://github.com/romanz/electrs), forked from [Blockstream/electrs](https://github.com/Blockstream/electrs) and adapted for [Meowcoin](https://mewccrypto.com) (Meowcoin Apex v30.2+).

Handles all three Meowcoin block formats: pre-KAWPOW (80-byte headers), KAWPOW/MEOWPOW (120-byte headers), and AuxPoW merge-mined blocks.

API documentation [is available here](https://github.com/blockstream/esplora/blob/master/API.md).

Documentation for the database schema and indexing process [is available here](doc/schema.md).

### Installing & indexing

Install Rust, Meowcoin Apex (no `txindex` needed) and the `clang` and `pkg-config` packages, increase maximum number of open files by `ulimit -n 100000` and then:

```bash
$ git clone https://github.com/Meowcoin-Foundation/electrs-mewc && cd electrs-mewc
$ cargo build --release
$ ./target/release/electrs --network mainnet --jsonrpc-import --daemon-rpc-addr 127.0.0.1:8332 --cookie "rpcuser:rpcpassword" -vvvv
```

> **Note:** Only `--jsonrpc-import` mode is supported. Direct blk*.dat file parsing is not available due to Meowcoin's variable-size block headers.

The indexes require significant storage (scale with chain size). Creating the full index from scratch takes several hours on a machine with SSD.

Prebuilt Linux x86_64 binaries are available on the [releases page](../../releases).

### Light mode

For personal or low-volume use, you may set `--lightmode` to reduce disk storage requirements
by roughly 50% at the cost of slower and more expensive lookups.

With this option set, raw transactions and metadata associated with blocks will not be kept in rocksdb
(the `T`, `X` and `M` indexes),
but instead queried from bitcoind on demand.

### Notable changes from Electrs:

- HTTP REST API in addition to the Electrum JSON-RPC protocol, with extended transaction information
  (previous outputs, spending transactions, script asm and more).

- Extended indexes and database storage for improved performance under high load:

  - A full transaction store mapping txids to raw transactions is kept in the database under the prefix `t`.
  - An index of all spendable transaction outputs is kept under the prefix `O`.
  - An index of all addresses (encoded as string) is kept under the prefix `a` to enable by-prefix address search.
  - A map of blockhash to txids is kept in the database under the prefix `X`.
  - Block stats metadata (number of transactions, size and weight) is kept in the database under the prefix `M`.

  With these new indexes, bitcoind is no longer queried to serve user requests and is only polled
  periodically for new blocks and for syncing the mempool.

- Meowcoin-native address encoding for all address types (P2PKH, P2SH, P2WPKH, P2WSH, P2TR) using Meowcoin's version bytes and bech32 HRPs (`mewc`/`tmewc`).

### CLI options

In addition to electrs's original configuration options, a few new options are also available:

- `--http-addr <addr:port>` - HTTP server address/port to listen on (default: `127.0.0.1:3000`).
- `--lightmode` - enable light mode (see above)
- `--cors <origins>` - origins allowed to make cross-site request (optional, defaults to none).
- `--address-search` - enables the by-prefix address search index.
- `--index-unspendables` - enables indexing of provably unspendable outputs.
- `--utxos-limit <num>` - maximum number of utxos to return per address.
- `--electrum-txs-limit <num>` - maximum number of txs to return per address in the electrum server (does not apply for the http api).
- `--electrum-banner <text>` - welcome banner text for electrum server.

Additional options with the `electrum-discovery` feature:
- `--electrum-hosts <json>` - a json map of the public hosts where the electrum server is reachable, in the [`server.features` format](https://electrumx.readthedocs.io/en/latest/protocol-methods.html#server.features).
- `--electrum-announce` - announce the electrum server on the electrum p2p server discovery network.

See `$ ./target/release/electrs --help` for the full list of options.

## License

MIT
