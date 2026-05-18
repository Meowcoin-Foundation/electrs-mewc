#!/usr/bin/env python3
"""
diff_verbose_tx.py — Validate `blockchain.transaction.get verbose=true`
parity between two Electrum servers.

Use case: confirm that electrs-mewc with --enable-verbose-transactions returns
the same shape as a reference ElectrumX server (which gets verbose responses
"for free" by proxying `getrawtransaction <txid> true` to a node with txindex).

Typical run:

    ./tools/diff_verbose_tx.py \\
        --reference  ssl://electrum.mewccrypto.com:50002 \\
        --candidate  ssl://electrs4.meowcoin.org:50002 \\
        --sample 50

What it does:
  1. Auto-samples confirmed txids by walking back from the chain tip and
     pulling tx ids by position (blockchain.transaction.id_from_pos).
  2. For each txid, fetches `blockchain.transaction.get <txid> true` from both
     servers.
  3. Diffs the two JSON responses, tolerating fields known to legitimately
     drift between independent servers (`confirmations`, since the two
     instances may be at different chain tips at the moment of measurement).
  4. Prints a pass/fail summary plus per-txid diffs.

Exits 0 if every sampled tx matches, 1 otherwise.

You can also pass --txids comma,separated,list to test specific transactions
(useful for spot-checking a coinbase tx, an asset-issuance tx, a swap-related
tx, or a mempool tx — for mempool entries the script automatically skips the
block-context fields, matching meowcoind's own behavior).
"""

from __future__ import annotations

import argparse
import json
import socket
import ssl
import sys
import time
import urllib.request
from dataclasses import dataclass
from typing import Any
from urllib.parse import urlparse

# ---------------------------------------------------------------------------
# Electrum client
# ---------------------------------------------------------------------------


@dataclass
class Endpoint:
    name: str
    host: str
    port: int
    use_ssl: bool


def parse_endpoint(name: str, uri: str) -> Endpoint:
    """Parse a URI like `ssl://host:50002` or `tcp://host:50001`."""
    parsed = urlparse(uri)
    if parsed.scheme not in ("ssl", "tls", "tcp"):
        raise ValueError(
            f"--{name} must use scheme ssl:// or tcp:// (got {parsed.scheme!r})"
        )
    if not parsed.hostname or not parsed.port:
        raise ValueError(f"--{name} must include host and port (got {uri!r})")
    return Endpoint(
        name=name,
        host=parsed.hostname,
        port=parsed.port,
        use_ssl=parsed.scheme in ("ssl", "tls"),
    )


class ElectrumClient:
    def __init__(self, endpoint: Endpoint, *, insecure: bool, timeout: float):
        self.endpoint = endpoint
        sock = socket.create_connection(
            (endpoint.host, endpoint.port), timeout=timeout
        )
        if endpoint.use_ssl:
            ctx = ssl.create_default_context()
            if insecure:
                ctx.check_hostname = False
                ctx.verify_mode = ssl.CERT_NONE
            sock = ctx.wrap_socket(sock, server_hostname=endpoint.host)
        self.sock = sock
        self.fd = sock.makefile("rwb")
        self.id = 0

    def call(self, method: str, *params: Any) -> Any:
        self.id += 1
        req = {"id": self.id, "method": method, "params": list(params)}
        line = (json.dumps(req) + "\n").encode("ascii")
        self.fd.write(line)
        self.fd.flush()
        # Loop until we get the response with our id. Server-initiated
        # subscription notifications (e.g. blockchain.headers.subscribe firing
        # when a new block arrives) come in on the same socket and must be
        # discarded rather than treated as the response to our request.
        while True:
            raw = self.fd.readline()
            if not raw:
                raise RuntimeError(
                    f"{self.endpoint.name} closed connection while waiting for "
                    f"{method}{tuple(params)}"
                )
            try:
                resp = json.loads(raw.decode("ascii"))
            except json.JSONDecodeError as e:
                raise RuntimeError(
                    f"{self.endpoint.name} returned non-JSON for "
                    f"{method}{tuple(params)}: {raw!r} ({e})"
                )
            if not isinstance(resp, dict):
                raise RuntimeError(
                    f"{self.endpoint.name} returned non-object for "
                    f"{method}{tuple(params)}: {resp!r}"
                )
            # Notifications have a `method` key but no matching `id`. Discard
            # them and keep reading.
            if resp.get("id") != self.id:
                if "method" in resp:
                    continue
                raise RuntimeError(
                    f"{self.endpoint.name} returned response with mismatched id "
                    f"(expected {self.id}, got {resp.get('id')!r}) for "
                    f"{method}{tuple(params)}: {resp!r}"
                )
            if resp.get("error") is not None:
                raise RuntimeError(
                    f"{self.endpoint.name} {method}{tuple(params)} -> {resp['error']}"
                )
            if "result" not in resp:
                raise RuntimeError(
                    f"{self.endpoint.name} returned no result for "
                    f"{method}{tuple(params)}: {resp!r}"
                )
            return resp["result"]

    def close(self) -> None:
        try:
            self.fd.close()
        finally:
            self.sock.close()


# ---------------------------------------------------------------------------
# Sampling
# ---------------------------------------------------------------------------


def fetch_mempool_txids(api_url: str, limit: int, timeout: float) -> list[str]:
    """Fetch current mempool txids from an explorer JSON API.

    Expects the endpoint to return a JSON array of txid strings. We use this to
    exercise the unconfirmed-tx code path in our verbose handler (which should
    omit blockhash/height/confirmations/time/blocktime, matching meowcoind's
    own behavior for unconfirmed txs).

    A custom User-Agent is set because the default Python urllib UA is
    commonly blocked by Cloudflare / WAFs in front of public explorers.
    """
    print(f"[mempool] GET {api_url}")
    req = urllib.request.Request(
        api_url, headers={"User-Agent": "diff_verbose_tx/1.0"}
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        data = json.loads(resp.read().decode("utf-8"))
    if not isinstance(data, list):
        raise RuntimeError(f"mempool API returned non-list: {type(data).__name__}")
    txids = [t for t in data if isinstance(t, str)]
    if not txids:
        print("[mempool] empty mempool — skipping unconfirmed coverage")
        return []
    if limit > 0 and len(txids) > limit:
        txids = txids[:limit]
    print(f"[mempool] using {len(txids)} mempool txid(s)")
    return txids


def sample_txids(
    client: ElectrumClient,
    count: int,
    *,
    max_blocks: int = 200,
    spread: int = 1,
    coinbase_only: bool = False,
) -> list[str]:
    """Walk back from the tip collecting txids until we have `count`.

    `spread` controls block stride: spread=1 walks adjacent blocks, spread=10
    skips 10 blocks between samples for wider chain coverage.

    `coinbase_only` restricts to position-0 (the coinbase tx) so we exercise
    the coinbase-style vin shape (which has `coinbase` + `sequence` instead
    of `txid` + `vout` + `scriptSig`).
    """
    tip = client.call("blockchain.headers.subscribe")
    tip_height = int(tip["height"])
    print(f"[sample] {client.endpoint.name} tip = {tip_height}")

    positions = (0,) if coinbase_only else (0, 1, 2, 3, 5, 10)

    txids: list[str] = []
    seen: set[str] = set()
    height = tip_height
    blocks_examined = 0

    while len(txids) < count and blocks_examined < max_blocks and height > 0:
        for pos in positions:
            try:
                txid = client.call(
                    "blockchain.transaction.id_from_pos", height, pos, False
                )
            except RuntimeError:
                # tx_pos out of range for this block; move on
                continue
            if not isinstance(txid, str):
                continue
            if txid in seen:
                continue
            seen.add(txid)
            txids.append(txid)
            if len(txids) >= count:
                break
        height -= spread
        blocks_examined += 1

    return txids


# ---------------------------------------------------------------------------
# Diffing
# ---------------------------------------------------------------------------


# Fields that may legitimately differ between two independent servers
# observed at slightly different times. We compare these with tolerance, not
# strict equality.
DRIFT_TOLERANT_FIELDS = {"confirmations"}

# Fields whose presence depends on whether the tx is confirmed. They should be
# present together or absent together; this invariant we check.
#
# `height` is here because ElectrumX Meowcoin (the public reference server,
# e.g. electrum.mewccrypto.com) injects it alongside the other block-context
# fields, even though it's not part of standard bitcoind getrawtransaction
# output. Our electrs-mewc implementation mirrors that behavior.
CONFIRMED_ONLY_FIELDS = {"blockhash", "height", "confirmations", "time", "blocktime"}

# `in_active_chain` is only included by bitcoind/meowcoind when the caller
# explicitly passes a blockhash to getrawtransaction; ElectrumX doesn't, so
# both servers should omit it. If it appears, ignore it during the diff.
IGNORED_FIELDS = {"in_active_chain"}


def normalize(obj: Any) -> Any:
    """Drop ignored fields recursively."""
    if isinstance(obj, dict):
        return {k: normalize(v) for k, v in obj.items() if k not in IGNORED_FIELDS}
    if isinstance(obj, list):
        return [normalize(v) for v in obj]
    return obj


def diff(
    ref: Any,
    cand: Any,
    *,
    confirmations_tolerance: int,
    path: str = "",
) -> list[str]:
    """Return a list of human-readable mismatch descriptions."""
    issues: list[str] = []

    if type(ref) is not type(cand):
        issues.append(f"{path or '<root>'}: type {type(ref).__name__} vs {type(cand).__name__}")
        return issues

    if isinstance(ref, dict):
        ref_keys = set(ref.keys())
        cand_keys = set(cand.keys())
        for k in sorted(ref_keys - cand_keys):
            issues.append(f"{path}.{k}: present in reference, missing in candidate")
        for k in sorted(cand_keys - ref_keys):
            issues.append(f"{path}.{k}: missing in reference, present in candidate")
        for k in sorted(ref_keys & cand_keys):
            sub_path = f"{path}.{k}" if path else k
            if k in DRIFT_TOLERANT_FIELDS and isinstance(ref[k], (int, float)):
                if abs(ref[k] - cand[k]) > confirmations_tolerance:
                    issues.append(
                        f"{sub_path}: drift {ref[k]} vs {cand[k]} "
                        f"(exceeds tolerance {confirmations_tolerance})"
                    )
                continue
            issues.extend(
                diff(
                    ref[k], cand[k],
                    confirmations_tolerance=confirmations_tolerance,
                    path=sub_path,
                )
            )
        return issues

    if isinstance(ref, list):
        if len(ref) != len(cand):
            issues.append(f"{path or '<root>'}: list length {len(ref)} vs {len(cand)}")
            return issues
        for i, (a, b) in enumerate(zip(ref, cand)):
            issues.extend(
                diff(
                    a, b,
                    confirmations_tolerance=confirmations_tolerance,
                    path=f"{path}[{i}]",
                )
            )
        return issues

    if ref != cand:
        issues.append(f"{path or '<root>'}: {ref!r} != {cand!r}")
    return issues


def check_confirmed_invariant(label: str, obj: dict) -> list[str]:
    """Either all 5 confirmed-only fields are present, or all 5 are absent."""
    present = CONFIRMED_ONLY_FIELDS & set(obj.keys())
    if 0 < len(present) < len(CONFIRMED_ONLY_FIELDS):
        return [
            f"{label}: confirmed-context fields are partially present "
            f"(have {sorted(present)}, missing {sorted(CONFIRMED_ONLY_FIELDS - present)})"
        ]
    return []


# ---------------------------------------------------------------------------
# KDF-focused validation
# ---------------------------------------------------------------------------
#
# Komodo DeFi Framework's `Transaction` deserializer
# (mm2_bitcoin/rpc/src/v1/types/transaction.rs) tells us exactly which fields
# matter for atomic-swap validation. The two modes below operate on that
# evidence rather than on bitcoind's full getrawtransaction surface.

# Fields KDF requires at top level (no #[serde(default)], will fail to
# deserialize if missing).
KDF_REQUIRED_TOP = ("hex", "txid", "version", "locktime", "vin", "vout")

# Fields KDF accepts as Option<> at top level (won't fail to deserialize
# without them, but populated when we have the data).
KDF_OPTIONAL_TOP = ("hash", "size", "vsize")

# Fields KDF reads via #[serde(default)] for confirmed txs.
KDF_CONFIRMED_TOP = ("blockhash", "confirmations", "time", "blocktime", "height")


def kdf_validate(label: str, resp: dict) -> list[str]:
    """Verify that `resp` contains everything KDF will read for swap validation."""
    issues: list[str] = []

    if not isinstance(resp, dict):
        return [f"{label}: response is not an object"]

    for field in KDF_REQUIRED_TOP:
        if field not in resp:
            issues.append(f"{label}: missing required field {field!r}")

    if "version" in resp and not isinstance(resp["version"], int):
        issues.append(f"{label}: version is not int (got {type(resp['version']).__name__})")
    if "locktime" in resp and not isinstance(resp["locktime"], int):
        issues.append(f"{label}: locktime is not int")
    if "hex" in resp and not isinstance(resp["hex"], str):
        issues.append(f"{label}: hex is not a string")
    if "txid" in resp and not isinstance(resp["txid"], str):
        issues.append(f"{label}: txid is not a string")

    # vin entries: either coinbase or signed
    if isinstance(resp.get("vin"), list):
        for i, vin in enumerate(resp["vin"]):
            if not isinstance(vin, dict):
                issues.append(f"{label}: vin[{i}] is not an object")
                continue
            if "coinbase" in vin:
                if "sequence" not in vin:
                    issues.append(f"{label}: vin[{i}] coinbase form missing sequence")
            else:
                for f in ("txid", "vout", "scriptSig", "sequence"):
                    if f not in vin:
                        issues.append(f"{label}: vin[{i}] missing {f!r}")
                if isinstance(vin.get("scriptSig"), dict):
                    for f in ("asm", "hex"):
                        if f not in vin["scriptSig"]:
                            issues.append(f"{label}: vin[{i}].scriptSig missing {f!r}")

    # vout entries: n + scriptPubKey { asm, hex, type }
    if isinstance(resp.get("vout"), list):
        for i, vout in enumerate(resp["vout"]):
            if not isinstance(vout, dict):
                issues.append(f"{label}: vout[{i}] is not an object")
                continue
            if "n" not in vout:
                issues.append(f"{label}: vout[{i}] missing n")
            spk = vout.get("scriptPubKey")
            if not isinstance(spk, dict):
                issues.append(f"{label}: vout[{i}] missing scriptPubKey")
            else:
                for f in ("asm", "hex", "type"):
                    if f not in spk:
                        issues.append(f"{label}: vout[{i}].scriptPubKey missing {f!r}")

    # confirmed-context fields: all-or-nothing
    confirmed = {f for f in KDF_CONFIRMED_TOP if f in resp}
    if confirmed and confirmed != set(KDF_CONFIRMED_TOP):
        missing = set(KDF_CONFIRMED_TOP) - confirmed
        issues.append(
            f"{label}: partial confirmed-context fields (missing {sorted(missing)})"
        )

    return issues


def kdf_cross_check(
    ref: dict, cand: dict, *, conf_tolerance: int
) -> list[str]:
    """Compare KDF-meaningful fields between two server responses.

    Strict equality on the canonical fields (hex, txid, version, locktime,
    blockhash, height, time, blocktime, vin per-input, vout per-output) and
    drift-tolerant comparison on confirmations.

    Cosmetic format differences (valueSat, addresses[], reqSigs, address,
    desc, empty txinwitness) are intentionally ignored — KDF doesn't read
    those fields, and they vary by daemon version.
    """
    issues: list[str] = []

    # Identity fields must match exactly.
    for field in ("txid", "hex", "version", "locktime"):
        if ref.get(field) != cand.get(field):
            issues.append(
                f"{field}: ref={ref.get(field)!r} cand={cand.get(field)!r}"
            )

    # Confirmed-block context: should match exactly when both servers agree
    # the tx is confirmed.
    ref_confirmed = "blockhash" in ref
    cand_confirmed = "blockhash" in cand
    if ref_confirmed != cand_confirmed:
        issues.append(
            f"confirmation status: ref_confirmed={ref_confirmed} cand_confirmed={cand_confirmed}"
        )
    elif ref_confirmed:
        for field in ("blockhash", "height", "time", "blocktime"):
            if ref.get(field) != cand.get(field):
                issues.append(
                    f"{field}: ref={ref.get(field)!r} cand={cand.get(field)!r}"
                )
        if "confirmations" in ref and "confirmations" in cand:
            drift = abs(int(ref["confirmations"]) - int(cand["confirmations"]))
            if drift > conf_tolerance:
                issues.append(
                    f"confirmations drift: ref={ref['confirmations']} "
                    f"cand={cand['confirmations']} (>{conf_tolerance})"
                )

    # vin: same length, same per-input core fields.
    rv, cv = ref.get("vin", []), cand.get("vin", [])
    if len(rv) != len(cv):
        issues.append(f"vin length: ref={len(rv)} cand={len(cv)}")
    else:
        for i, (a, b) in enumerate(zip(rv, cv)):
            if not isinstance(a, dict) or not isinstance(b, dict):
                issues.append(f"vin[{i}]: non-object")
                continue
            if "coinbase" in a or "coinbase" in b:
                if a.get("coinbase") != b.get("coinbase"):
                    issues.append(
                        f"vin[{i}].coinbase: ref={a.get('coinbase')!r} cand={b.get('coinbase')!r}"
                    )
                if a.get("sequence") != b.get("sequence"):
                    issues.append(
                        f"vin[{i}].sequence: ref={a.get('sequence')} cand={b.get('sequence')}"
                    )
            else:
                for f in ("txid", "vout", "sequence"):
                    if a.get(f) != b.get(f):
                        issues.append(
                            f"vin[{i}].{f}: ref={a.get(f)!r} cand={b.get(f)!r}"
                        )
                ra = a.get("scriptSig") or {}
                rb = b.get("scriptSig") or {}
                for f in ("asm", "hex"):
                    if ra.get(f) != rb.get(f):
                        issues.append(f"vin[{i}].scriptSig.{f}: differs")

    # vout: same length, same per-output core fields.
    rv, cv = ref.get("vout", []), cand.get("vout", [])
    if len(rv) != len(cv):
        issues.append(f"vout length: ref={len(rv)} cand={len(cv)}")
    else:
        for i, (a, b) in enumerate(zip(rv, cv)):
            if not isinstance(a, dict) or not isinstance(b, dict):
                issues.append(f"vout[{i}]: non-object")
                continue
            if a.get("n") != b.get("n"):
                issues.append(f"vout[{i}].n: ref={a.get('n')} cand={b.get('n')}")
            ra_v, rb_v = a.get("value"), b.get("value")
            if ra_v != rb_v:
                # Tolerate float representation differences within 1 sat.
                if (
                    isinstance(ra_v, (int, float))
                    and isinstance(rb_v, (int, float))
                    and abs(float(ra_v) - float(rb_v)) <= 1e-8
                ):
                    pass
                else:
                    issues.append(f"vout[{i}].value: ref={ra_v!r} cand={rb_v!r}")
            ra = a.get("scriptPubKey") or {}
            rb = b.get("scriptPubKey") or {}
            for f in ("asm", "hex", "type"):
                if ra.get(f) != rb.get(f):
                    issues.append(
                        f"vout[{i}].scriptPubKey.{f}: ref={ra.get(f)!r} cand={rb.get(f)!r}"
                    )

    return issues


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--reference", required=True, help="ssl://host:port or tcp://host:port — ground truth (e.g. ElectrumX)")
    parser.add_argument("--candidate", required=True, help="ssl://host:port or tcp://host:port — server under test (electrs-mewc)")
    parser.add_argument("--sample", type=int, default=20, help="Auto-sample this many confirmed txids from recent blocks")
    parser.add_argument("--txids", help="Comma-separated txids to test (in addition to --sample)")
    parser.add_argument("--max-blocks", type=int, default=400, help="Walk back at most this many blocks looking for txids")
    parser.add_argument("--spread", type=int, default=1, help="Block stride between samples; >1 spreads samples across a wider chain range")
    parser.add_argument("--coinbase-only", action="store_true", help="Only sample coinbase transactions (position 0 of each block)")
    parser.add_argument("--mempool-api", default="https://explorer.mewccrypto.com/api/mempool/txids", help="Explorer endpoint returning a JSON list of current mempool txids; set to empty to disable")
    parser.add_argument("--mempool-limit", type=int, default=10, help="Max mempool txids to include from --mempool-api (0 = all)")
    parser.add_argument("--kdf", action="store_true", help="KDF-focused mode: validate the fields KDF actually reads on each server, and cross-check those fields between servers (instead of strict full-shape diff)")
    parser.add_argument("--confirmations-tolerance", type=int, default=3, help="Allow `confirmations` to drift by this many between servers (default: 3)")
    parser.add_argument("--insecure", action="store_true", help="Skip TLS cert verification (for self-signed reference servers)")
    parser.add_argument("--timeout", type=float, default=15.0, help="Per-RPC timeout in seconds")
    parser.add_argument("--verbose", "-v", action="store_true", help="Print full responses on mismatch")
    parser.add_argument("--show-shape", action="store_true", help="On the first tx, print the field inventory of the reference response (for sanity checking)")
    args = parser.parse_args()

    ref_ep = parse_endpoint("reference", args.reference)
    cand_ep = parse_endpoint("candidate", args.candidate)

    print(f"[connect] reference: {args.reference}")
    print(f"[connect] candidate: {args.candidate}")
    ref = ElectrumClient(ref_ep, insecure=args.insecure, timeout=args.timeout)
    cand = ElectrumClient(cand_ep, insecure=args.insecure, timeout=args.timeout)

    try:
        # Sanity check: both servers respond.
        ref_version = ref.call("server.version", "diff_verbose_tx", "1.4")
        cand_version = cand.call("server.version", "diff_verbose_tx", "1.4")
        print(f"[version] reference: {ref_version}")
        print(f"[version] candidate: {cand_version}")

        # Build the txid list.
        txids: list[str] = []
        if args.sample > 0:
            sampled = sample_txids(
                ref,
                args.sample,
                max_blocks=args.max_blocks,
                spread=args.spread,
                coinbase_only=args.coinbase_only,
            )
            print(f"[sample] collected {len(sampled)} txids from reference")
            txids.extend(sampled)
        if args.mempool_api:
            try:
                mempool_txids = fetch_mempool_txids(
                    args.mempool_api, args.mempool_limit, args.timeout
                )
                txids.extend(mempool_txids)
            except Exception as e:
                print(f"[mempool] WARN failed to fetch mempool txids: {e}")
        if args.txids:
            txids.extend(t.strip() for t in args.txids.split(",") if t.strip())

        if not txids:
            print("[error] no txids to test (try --sample N or --txids ...)", file=sys.stderr)
            return 2

        # Run the diff.
        passes = 0
        fails = 0
        for i, txid in enumerate(txids, 1):
            try:
                t0 = time.time()
                r_resp = ref.call("blockchain.transaction.get", txid, True)
                cand_dt = time.time()
                c_resp = cand.call("blockchain.transaction.get", txid, True)
                done_dt = time.time()
            except RuntimeError as e:
                print(f"[{i}/{len(txids)}] {txid}  ERROR  {e}")
                fails += 1
                continue

            if args.show_shape and i == 1:
                print("[shape] reference response top-level keys:")
                for k in sorted(r_resp.keys()):
                    v = r_resp[k]
                    sample = repr(v)[:60] + ("..." if len(repr(v)) > 60 else "")
                    print(f"  {k}: ({type(v).__name__}) {sample}")

            r_norm = normalize(r_resp)
            c_norm = normalize(c_resp)

            issues: list[str] = []
            if args.kdf:
                # KDF mode: validate each server independently against KDF's
                # parser requirements, then cross-check only the fields that
                # KDF actually reads.
                issues.extend(kdf_validate("reference", r_norm))
                issues.extend(kdf_validate("candidate", c_norm))
                issues.extend(kdf_cross_check(
                    r_norm, c_norm,
                    conf_tolerance=args.confirmations_tolerance,
                ))
            else:
                # Strict mode: full-shape diff (will flag cosmetic daemon-version
                # differences).
                issues.extend(check_confirmed_invariant("reference", r_norm))
                issues.extend(check_confirmed_invariant("candidate", c_norm))
                issues.extend(diff(
                    r_norm, c_norm,
                    confirmations_tolerance=args.confirmations_tolerance,
                ))

            ref_ms = (cand_dt - t0) * 1000
            cand_ms = (done_dt - cand_dt) * 1000
            tag = "OK  " if not issues else "FAIL"
            print(
                f"[{i}/{len(txids)}] {txid}  {tag}  "
                f"ref={ref_ms:.0f}ms cand={cand_ms:.0f}ms "
                f"(issues: {len(issues)})"
            )
            if issues:
                fails += 1
                for issue in issues:
                    print(f"    - {issue}")
                if args.verbose:
                    print("    --- reference ---")
                    print("   ", json.dumps(r_resp, indent=2).replace("\n", "\n    "))
                    print("    --- candidate ---")
                    print("   ", json.dumps(c_resp, indent=2).replace("\n", "\n    "))
            else:
                passes += 1

        print()
        print(f"[summary] {passes}/{len(txids)} passed, {fails} failed")
        return 0 if fails == 0 else 1
    finally:
        ref.close()
        cand.close()


if __name__ == "__main__":
    sys.exit(main())
