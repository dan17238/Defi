#!/usr/bin/env python3
"""Background blockchain data fetcher for Arbitrum liquidation dashboard."""

import json, time, threading, http.client, ssl, struct

# ─── Constants ───

RPC_HOST = 'arb1.arbitrum.io'
RPC_PORT = 443
RPC_PATH = '/rpc'

AAVE_V3_POOL = '0x794a61358D6845594F94dc1DB02A252b5b4814aD'.lower()
RADIANT_POOL = '0xF4B1486DD74D07706052A33d31d7c0AAFD0659E1'.lower()
ETH_USD_APPROX = 3500.0

# LiquidationCall(address,address,address,uint256,uint256,address,bool)
LIQUIDATION_TOPIC = '0xe413a321e8681d831f4dbccbca790d2952b56f977908e45be37335533e005286'

# AAVE v3 Borrow(address,address,address,uint256,uint8,uint256,uint16)
AAVE_BORROW_TOPIC = '0xb3d084820fb1a9decffb176436bd02558d15fac9b0ddfed8c465bc7359d7dce0'

# Radiant (AAVE v2 fork) Borrow(address,address,address,uint256,uint256,uint256,uint16)
RADIANT_BORROW_TOPIC = '0xc6a898309e823ee50bac64e45ca8adba6690e99e7841c45d754e2a38e9019d9b'

MULTICALL3 = '0xcA11bde05977b3631167028862bE2a173976CA11'
AGGREGATE3_SELECTOR = '0x82ad56cb'
GET_USER_ACCOUNT_DATA_SELECTOR = '0xbf92857c'

SCAN_BLOCKS = 500000  # ~35 hours, covers more liquidation events
MAX_LIQUIDATIONS = 50
MAX_BORROWERS = 200
HEALTH_FACTOR_THRESHOLD = 1.5
UPDATE_INTERVAL = 30

TOKENS = {
    "0x82af49447d8a07e3bd95bd0d56f35241523fbab1": ("WETH", 18),
    "0xaf88d065e77c8cc2239327c5edb3a432268e5831": ("USDC", 6),
    "0xff970a61a04b1ca14834a43f5de4533ebddb5cc8": ("USDC.e", 6),
    "0xfd086bc7cd5c481dcc9c85ebe478a1c0b69fcbb9": ("USDT", 6),
    "0xda10009cbd5d07dd0cecc66161fc93d7c9000da1": ("DAI", 18),
    "0x2f2a2543b76a4166549f7aab2e75bef0aefc5b0f": ("WBTC", 8),
    "0x912ce59144191c1204e64559fe8253a0e49e6548": ("ARB", 18),
    "0xfc5a1a6eb076a2c7ad06ed22c90d7e710e35ad0a": ("GMX", 18),
    "0xf97f4df75117a78c1a5a0dbb814af92458539fb4": ("LINK", 18),
    "0x5979d7b546e38e9ab5011956dea6f53c2ba11622": ("wstETH", 18),
    "0x539bde0d7dbd336b79148aa742883198bbf60342": ("MAGIC", 18),
    "0x17fc002b466eec40dae837fc4be5c67993ddbd6f": ("FRAX", 18),
}

PROTOCOL_POOLS = {
    'AAVE V3': AAVE_V3_POOL,
    'Radiant': RADIANT_POOL,
}

# ─── Shared state ───

chain_state = {
    'market_liquidations': [],
    'competitors': [],
    'near_liquidation': [],
    'last_updated': 0,
}

_lock = threading.Lock()
_conn = None


# ─── RPC helpers ───

def _get_conn():
    """Get or create a persistent HTTPS connection."""
    global _conn
    if _conn is not None:
        return _conn
    try:
        ctx = ssl.create_default_context()
        _conn = http.client.HTTPSConnection(RPC_HOST, RPC_PORT, timeout=15, context=ctx)
        return _conn
    except Exception:
        _conn = None
        return None


def _rpc_call(method, params=None):
    """Make a JSON-RPC call, return the result or None on failure."""
    global _conn
    if params is None:
        params = []
    body = json.dumps({"jsonrpc": "2.0", "method": method, "params": params, "id": 1}).encode()
    try:
        conn = _get_conn()
        if not conn:
            return None
        conn.request("POST", RPC_PATH, body, {"Content-Type": "application/json"})
        resp = conn.getresponse()
        data = resp.read()
        result = json.loads(data)
        if 'error' in result:
            return None
        return result.get('result')
    except Exception:
        _conn = None
        return None


def _get_block_number():
    """Get current block number."""
    result = _rpc_call("eth_blockNumber")
    if result:
        return int(result, 16)
    return None


# ─── ABI helpers ───

def _pad_address(addr):
    """Left-pad an address to 32 bytes (64 hex chars)."""
    clean = addr.lower().replace('0x', '')
    return clean.zfill(64)


def _topic_to_address(topic):
    """Extract address from a 32-byte topic (last 20 bytes)."""
    clean = topic.replace('0x', '')
    return '0x' + clean[-40:]


def _decode_uint256(hex_str):
    """Decode a 32-byte hex string as uint256."""
    return int(hex_str, 16)


def _shorten_address(addr):
    """Shorten address to 0x1234...5678 format."""
    if not addr or len(addr) < 12:
        return addr or '???'
    return addr[:6] + '...' + addr[-4:]


def _token_symbol(addr):
    """Get token symbol for address."""
    clean = addr.lower()
    if clean in TOKENS:
        return TOKENS[clean][0]
    return _shorten_address(addr)


def _token_decimals(addr):
    """Get token decimals for address."""
    clean = addr.lower()
    if clean in TOKENS:
        return TOKENS[clean][1]
    return 18


def _format_amount(raw_value, decimals):
    """Format a raw uint256 amount with proper decimals."""
    if raw_value == 0:
        return "0"
    value = raw_value / (10 ** decimals)
    if value >= 1_000_000:
        return f"{value:,.0f}"
    elif value >= 1_000:
        return f"{value:,.2f}"
    elif value >= 1:
        return f"{value:.4f}"
    elif value >= 0.0001:
        return f"{value:.6f}"
    else:
        return f"{value:.8f}"


def _format_usd(raw_value):
    """Format a raw uint256 from AAVE (expressed in base units = 8 decimals for USD values, but
    getUserAccountData returns values in ETH with 18 decimals, or USD with different bases).
    AAVE getUserAccountData returns values in ETH base units (18 decimals)."""
    # AAVE returns totalCollateralETH, totalDebtETH in base currency units (USD with 8 decimals on Arbitrum)
    # On Arbitrum AAVE v3, the base currency is USD with 8 decimals
    value = raw_value / (10 ** 8)
    if value >= 1_000_000:
        return f"${value:,.0f}"
    elif value >= 1_000:
        return f"${value:,.0f}"
    else:
        return f"${value:,.2f}"


def _format_eth_base_value_as_usd(raw_value):
    """Format a Radiant ETH-denominated account value as approximate USD."""
    value_eth = raw_value / (10 ** 18)
    value_usd = value_eth * ETH_USD_APPROX
    if value_usd >= 1_000_000:
        return f"${value_usd:,.0f}"
    elif value_usd >= 1_000:
        return f"${value_usd:,.0f}"
    else:
        return f"${value_usd:,.2f}"


def _time_ago(block_number, current_block):
    """Estimate time ago from block difference (Arbitrum ~250ms blocks)."""
    diff = current_block - block_number
    seconds = diff * 0.25
    if seconds < 60:
        return f"{int(seconds)}s ago"
    elif seconds < 3600:
        return f"{int(seconds / 60)}m ago"
    elif seconds < 86400:
        return f"{seconds / 3600:.1f}h ago"
    else:
        return f"{seconds / 86400:.1f}d ago"


# ─── Log fetching ───

def _get_logs(address, topics, from_block, to_block):
    """Fetch logs for a given address and topics. Returns list of log dicts or empty list."""
    params = [{
        "address": address,
        "topics": topics,
        "fromBlock": hex(from_block),
        "toBlock": hex(to_block),
    }]
    result = _rpc_call("eth_getLogs", params)
    if result is None:
        return []
    if isinstance(result, list):
        return result
    return []


def _fetch_liquidation_logs(current_block):
    """Fetch LiquidationCall events from AAVE v3 and Radiant, in batches."""
    from_block = max(0, current_block - SCAN_BLOCKS)
    all_logs = []
    batch_size = 100000  # Safe batch size for public RPCs

    for protocol, address in [('AAVE V3', AAVE_V3_POOL), ('Radiant', RADIANT_POOL)]:
        start = from_block
        while start < current_block:
            end = min(start + batch_size, current_block)
            logs = _get_logs(address, [LIQUIDATION_TOPIC], start, end)
            for log in logs:
                log['_protocol'] = protocol
            all_logs.extend(logs)
            start = end + 1

    return all_logs


def _parse_liquidation_event(log, current_block):
    """Parse a LiquidationCall event log into a structured dict."""
    try:
        topics = log.get('topics', [])
        if len(topics) < 4:
            return None

        collateral_asset = _topic_to_address(topics[1])
        debt_asset = _topic_to_address(topics[2])
        user = _topic_to_address(topics[3])

        # Decode data: debtToCover (uint256), liquidatedCollateralAmount (uint256),
        #              liquidator (address), receiveAToken (bool)
        data = log.get('data', '0x').replace('0x', '')
        if len(data) < 256:  # Need at least 4 * 64 hex chars
            return None

        chunks = [data[i:i+64] for i in range(0, len(data), 64)]
        debt_to_cover = _decode_uint256(chunks[0])
        liquidated_collateral = _decode_uint256(chunks[1])
        liquidator = '0x' + chunks[2][-40:]

        block_hex = log.get('blockNumber', '0x0')
        block_num = int(block_hex, 16) if isinstance(block_hex, str) else block_hex
        tx_hash = log.get('transactionHash', '0x')

        collateral_sym = _token_symbol(collateral_asset)
        debt_sym = _token_symbol(debt_asset)
        collateral_dec = _token_decimals(collateral_asset)
        debt_dec = _token_decimals(debt_asset)

        return {
            'protocol': log.get('_protocol', '???'),
            'block': block_num,
            'tx_hash': tx_hash,
            'user': _shorten_address(user),
            'user_full': user,
            'liquidator': _shorten_address(liquidator),
            'liquidator_full': liquidator,
            'collateral': collateral_sym,
            'debt': debt_sym,
            'debt_amount': _format_amount(debt_to_cover, debt_dec),
            'collateral_amount': _format_amount(liquidated_collateral, collateral_dec),
            'time_ago': _time_ago(block_num, current_block),
        }
    except Exception:
        return None


# ─── Borrow event fetching ───

def _fetch_borrow_logs(current_block):
    """Fetch Borrow events from supported protocols to find active borrowers."""
    from_block = max(0, current_block - SCAN_BLOCKS)
    all_logs = []
    batch_size = 100000
    protocols = [
        ('AAVE V3', AAVE_V3_POOL, AAVE_BORROW_TOPIC),
        ('Radiant', RADIANT_POOL, RADIANT_BORROW_TOPIC),
    ]

    for protocol, pool, topic in protocols:
        start = from_block
        while start < current_block:
            end = min(start + batch_size, current_block)
            logs = _get_logs(pool, [topic], start, end)
            for log in logs:
                log['_protocol'] = protocol
                log['_pool'] = pool
            all_logs.extend(logs)
            start = end + 1
    return all_logs


def _extract_borrowers(borrow_logs, liquidation_events):
    """Extract unique (protocol, pool, borrower) triples in a stable priority order."""
    borrowers = []
    seen = set()

    def _block_number(value):
        if isinstance(value, str):
            try:
                return int(value, 16)
            except ValueError:
                return 0
        return int(value or 0)

    def _append(protocol, pool, addr):
        key = (protocol, pool, addr.lower())
        if key not in seen:
            seen.add(key)
            borrowers.append(key)

    # Most recently liquidated users are strongest candidates to watch first.
    for evt in sorted(liquidation_events, key=lambda x: x.get('block', 0), reverse=True):
        if evt and evt.get('user_full'):
            protocol = evt.get('protocol')
            pool = PROTOCOL_POOLS.get(protocol)
            if pool:
                _append(protocol, pool, evt['user_full'])

    # Then fill with the newest borrowers from supported protocols.
    def _borrow_sort_key(log):
        return (
            _block_number(log.get('blockNumber')),
            _block_number(log.get('transactionIndex')),
            _block_number(log.get('logIndex')),
        )

    for log in sorted(borrow_logs, key=_borrow_sort_key, reverse=True):
        topics = log.get('topics', [])
        if len(topics) >= 3:
            addr = _topic_to_address(topics[2])
            protocol = log.get('_protocol', 'AAVE V3')
            pool = log.get('_pool', PROTOCOL_POOLS.get(protocol, AAVE_V3_POOL))
            _append(protocol, pool, addr)
        if len(borrowers) >= MAX_BORROWERS:
            break

    return borrowers[:MAX_BORROWERS]


# ─── Multicall for health factors ───

def _encode_multicall_health_factors(addresses, target_pool):
    """Encode a multicall3 aggregate3 call for getUserAccountData on each address.

    aggregate3 signature: aggregate3((address target, bool allowFailure, bytes callData)[])
    Each Call3 struct: (address, bool, bytes)

    We ABI-encode the array of Call3 structs.
    """
    if not addresses:
        return None

    # Build individual call datas: getUserAccountData(address)
    # selector (4 bytes) + address padded to 32 bytes
    calls = []
    for addr in addresses:
        calldata = GET_USER_ACCOUNT_DATA_SELECTOR.replace('0x', '') + _pad_address(addr)
        calls.append({
            'target': target_pool,
            'allowFailure': True,
            'callData': calldata,
        })

    # Now ABI-encode the aggregate3 call
    # aggregate3(Call3[] calldata calls)
    # Call3 = (address target, bool allowFailure, bytes callData)

    # Function selector
    encoded = AGGREGATE3_SELECTOR.replace('0x', '')

    # Offset to the dynamic array (32 = 0x20)
    encoded += hex(32)[2:].zfill(64)

    # Array length
    n = len(calls)
    encoded += hex(n)[2:].zfill(64)

    # Each element is a tuple (address, bool, bytes) which is dynamic because of bytes
    # So we need offsets for each element first
    # Calculate offsets: each element offset points from start of array data

    # First, collect encoded tuples
    tuple_encodings = []
    for call in calls:
        # address (padded)
        t_enc = _pad_address(call['target'])
        # bool (padded)
        t_enc += hex(1 if call['allowFailure'] else 0)[2:].zfill(64)
        # offset to bytes data (always 96 = 0x60 since 3 words for address, bool, offset)
        t_enc += hex(96)[2:].zfill(64)
        # bytes length
        cd = call['callData']
        byte_len = len(cd) // 2
        t_enc += hex(byte_len)[2:].zfill(64)
        # bytes data padded to 32-byte boundary
        padded_len = ((len(cd) + 63) // 64) * 64
        t_enc += cd.ljust(padded_len, '0')
        tuple_encodings.append(t_enc)

    # Calculate offsets for each tuple
    # Offsets are relative to the start of the array elements area
    # Each tuple is dynamic, so array contains offsets first, then data
    offsets_area_size = n * 32  # n offsets, each 32 bytes
    current_data_offset = offsets_area_size

    offsets = []
    for t_enc in tuple_encodings:
        offsets.append(current_data_offset)
        current_data_offset += len(t_enc) // 2  # in bytes

    # Write offsets
    for off in offsets:
        encoded += hex(off)[2:].zfill(64)

    # Write tuple data
    for t_enc in tuple_encodings:
        encoded += t_enc

    return '0x' + encoded


def _decode_multicall_results(hex_data, num_calls):
    """Decode aggregate3 return data.

    Returns: list of (success: bool, health_factor: float, collateral_usd: int, debt_usd: int)
    for each call.

    aggregate3 returns: Result[] where Result = (bool success, bytes returnData)
    getUserAccountData returns: (totalCollateralBase, totalDebtBase, availableBorrowsBase,
                                  currentLiquidationThreshold, ltv, healthFactor)
    All are uint256. healthFactor has 18 decimals.
    """
    results = []
    if not hex_data or hex_data == '0x':
        return [(False, 0, 0, 0)] * num_calls

    data = hex_data.replace('0x', '')
    if len(data) < 64:
        return [(False, 0, 0, 0)] * num_calls

    try:
        # First 32 bytes: offset to array
        array_offset = _decode_uint256(data[0:64]) * 2  # convert to hex char offset
        # Array length
        array_len = _decode_uint256(data[array_offset:array_offset + 64])

        # Read offsets to each Result tuple
        result_offsets = []
        for i in range(min(array_len, num_calls)):
            off_pos = array_offset + 64 + i * 64
            off = _decode_uint256(data[off_pos:off_pos + 64])
            # Offset is relative to array_offset + 64 (start of array elements area)
            result_offsets.append(array_offset + 64 + off * 2)

        for roff in result_offsets:
            try:
                # Result = (bool success, bytes returnData)
                success = _decode_uint256(data[roff:roff + 64]) != 0
                # Offset to bytes
                bytes_offset_val = _decode_uint256(data[roff + 64:roff + 128])
                bytes_start = roff + bytes_offset_val * 2
                bytes_len = _decode_uint256(data[bytes_start:bytes_start + 64])

                if not success or bytes_len < 192:  # Need 6 uint256s = 192 bytes
                    results.append((False, 0, 0, 0))
                    continue

                # Parse getUserAccountData return values
                ret_start = bytes_start + 64
                total_collateral = _decode_uint256(data[ret_start:ret_start + 64])
                total_debt = _decode_uint256(data[ret_start + 64:ret_start + 128])
                # skip availableBorrows [128:192], liquidationThreshold [192:256], ltv [256:320]
                health_factor_raw = _decode_uint256(data[ret_start + 320:ret_start + 384])
                health_factor = health_factor_raw / (10 ** 18)

                results.append((True, health_factor, total_collateral, total_debt))
            except Exception:
                results.append((False, 0, 0, 0))

    except Exception:
        return [(False, 0, 0, 0)] * num_calls

    # Pad if we got fewer results
    while len(results) < num_calls:
        results.append((False, 0, 0, 0))

    return results


def _batch_health_factors(pool_address, addresses):
    """Query health factors for a list of addresses against a specific pool."""
    if not addresses:
        return []

    # Process in batches of 50 to avoid oversized calls
    batch_size = 50
    all_results = []

    for i in range(0, len(addresses), batch_size):
        batch = addresses[i:i + batch_size]
        calldata = _encode_multicall_health_factors(batch, pool_address)
        if not calldata:
            all_results.extend([(False, 0, 0, 0)] * len(batch))
            continue

        params = [{
            "to": MULTICALL3,
            "data": calldata,
        }, "latest"]

        result = _rpc_call("eth_call", params)
        if result:
            decoded = _decode_multicall_results(result, len(batch))
            all_results.extend(decoded)
        else:
            all_results.extend([(False, 0, 0, 0)] * len(batch))

    return all_results


# ─── Competitor analysis ───

def _analyze_competitors(liquidation_events):
    """Group liquidation events by liquidator address."""
    bots = {}
    for evt in liquidation_events:
        if not evt:
            continue
        addr = evt['liquidator_full'].lower()
        if addr not in bots:
            bots[addr] = {
                'address': addr,
                'short': _shorten_address(addr),
                'count': 0,
                'protocols': set(),
                'last_block': 0,
            }
        bots[addr]['count'] += 1
        bots[addr]['protocols'].add(evt['protocol'])
        bots[addr]['last_block'] = max(bots[addr]['last_block'], evt['block'])

    # Convert sets to lists and sort by count
    result = []
    for addr, info in bots.items():
        result.append({
            'address': info['address'],
            'short': info['short'],
            'count': info['count'],
            'protocols': sorted(info['protocols']),
            'last_block': info['last_block'],
        })

    result.sort(key=lambda x: x['count'], reverse=True)
    return result


# ─── Main update loop ───

def _do_update():
    """Perform one full update cycle."""
    current_block = _get_block_number()
    if current_block is None:
        return

    # 1) Fetch liquidation events
    raw_logs = _fetch_liquidation_logs(current_block)
    liquidation_events = []
    for log in raw_logs:
        evt = _parse_liquidation_event(log, current_block)
        if evt:
            liquidation_events.append(evt)

    # Sort by block descending (most recent first), take last MAX_LIQUIDATIONS
    liquidation_events.sort(key=lambda x: x['block'], reverse=True)
    liquidation_events = liquidation_events[:MAX_LIQUIDATIONS]

    # 2) Analyze competitors
    competitors = _analyze_competitors(liquidation_events)

    # 3) Near-liquidation positions
    # Fetch borrow events to find active borrowers
    borrow_logs = _fetch_borrow_logs(current_block)
    borrower_entries = _extract_borrowers(borrow_logs, liquidation_events)

    near_liquidation = []
    if borrower_entries:
        grouped = {}
        for protocol, pool, addr in borrower_entries:
            grouped.setdefault((protocol, pool), []).append(addr)

        for (protocol, pool), addresses in grouped.items():
            health_results = _batch_health_factors(pool, addresses)
            for addr, (success, hf, collateral, debt) in zip(addresses, health_results):
                if success and 0 < hf < HEALTH_FACTOR_THRESHOLD and debt > 0:
                    if protocol == 'Radiant':
                        collateral_usd = _format_eth_base_value_as_usd(collateral)
                        debt_usd = _format_eth_base_value_as_usd(debt)
                    else:
                        collateral_usd = _format_usd(collateral)
                        debt_usd = _format_usd(debt)

                    near_liquidation.append({
                        'protocol': protocol,
                        'user': _shorten_address(addr),
                        'user_full': addr,
                        'health_factor': f"{hf:.4f}",
                        'health_factor_num': hf,
                        'collateral_usd': collateral_usd,
                        'debt_usd': debt_usd,
                    })

    # Sort by health factor ascending (closest to liquidation first)
    near_liquidation.sort(key=lambda x: x['health_factor_num'])

    # Update shared state
    with _lock:
        chain_state['market_liquidations'] = liquidation_events
        chain_state['competitors'] = competitors
        chain_state['near_liquidation'] = near_liquidation
        chain_state['last_updated'] = time.time()


def _update_loop():
    """Background thread that periodically fetches chain data."""
    while True:
        try:
            _do_update()
        except Exception:
            pass  # Never crash
        time.sleep(UPDATE_INTERVAL)


def get_state():
    """Thread-safe read of current chain state."""
    with _lock:
        # Return a shallow copy with the data
        return {
            'market_liquidations': list(chain_state['market_liquidations']),
            'competitors': list(chain_state['competitors']),
            'near_liquidation': [
                {k: v for k, v in pos.items() if k != 'health_factor_num'}
                for pos in chain_state['near_liquidation']
            ],
            'last_updated': chain_state['last_updated'],
        }


def start():
    """Start the background update thread."""
    t = threading.Thread(target=_update_loop, daemon=True)
    t.start()
    return t
