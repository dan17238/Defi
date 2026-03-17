#!/usr/bin/env python3
"""Dashboard server with server-side latency probing."""

import json, time, threading, http.client, ssl, socketserver, os
from http.server import HTTPServer, BaseHTTPRequestHandler
from pathlib import Path
from urllib.parse import urlparse
import chain_data
import tomllib

def _repo_root():
    return Path(__file__).resolve().parent.parent


def _load_settings():
    config_path = Path(os.environ.get("APP_CONFIG_PATH", _repo_root() / "config/default.toml"))
    try:
        with config_path.open("rb") as f:
            return tomllib.load(f)
    except Exception:
        return {}


def _parse_target(url, default):
    if not url:
        return default
    parsed = urlparse(url)
    if not parsed.hostname:
        return default
    use_ssl = parsed.scheme == "https"
    port = parsed.port or (443 if use_ssl else 80)
    path = parsed.path or "/"
    return (parsed.hostname, port, path, use_ssl)


SETTINGS = _load_settings()
MONITORING = SETTINGS.get("monitoring", {})
RPC = SETTINGS.get("rpc", {})
SEQUENCER = SETTINGS.get("sequencer", {})

TARGETS = {
    'read': _parse_target(RPC.get('http_url'), ('arb1.arbitrum.io', 443, '/rpc', True)),
    'read_arb1': ('arb1.arbitrum.io', 443, '/rpc', True),
    'sequencer': _parse_target(SEQUENCER.get('rpc_url'), ('arb1-sequencer.arbitrum.io', 443, '/rpc', True)),
    'local_node': _parse_target(
        os.environ.get('LOCAL_NODE_RPC_URL')
        or (
            RPC.get('http_url')
            if urlparse(RPC.get('http_url', '')).hostname in ('127.0.0.1', 'localhost')
            else None
        ),
        ('127.0.0.1', 8547, '/', False),
    ),
}

RPC_BODY = json.dumps({"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}).encode()

latest = {
    'read': {'ms': -1, 'block': None},
    'read_arb1': {'ms': -1, 'block': None},
    'sequencer': {'ms': -1, 'block': None},
    'local_node': {'ms': -1, 'block': None},
    'timestamp': 0,
}
history = {'read': [], 'sequencer': [], 'total': []}
MAX_HISTORY = 120
conns = {}
BOT_API_BASE = os.environ.get(
    'BOT_API_BASE',
    f"http://127.0.0.1:{MONITORING.get('dashboard_port', 3001)}",
)
SERVER_PORT = int(os.environ.get('PY_DASHBOARD_PORT', MONITORING.get('python_dashboard_port', 3000)))

# Read HTML once at startup
HTML_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), 'index.html')
with open(HTML_PATH, 'rb') as f:
    HTML_BYTES = f.read()

def get_conn(host, port, use_ssl=True):
    key = f"{host}:{port}:{use_ssl}"
    if key in conns:
        return conns[key]
    try:
        if use_ssl:
            conn = http.client.HTTPSConnection(host, port, timeout=5, context=ssl.create_default_context())
        else:
            conn = http.client.HTTPConnection(host, port, timeout=3)
        conns[key] = conn
        return conn
    except:
        return None

def probe_target(host, port, path, use_ssl=True):
    key = f"{host}:{port}"
    try:
        conn = get_conn(host, port, use_ssl)
        if not conn:
            return -1, None
        t0 = time.perf_counter()
        conn.request("POST", path, RPC_BODY, {"Content-Type": "application/json"})
        resp = conn.getresponse()
        body = resp.read()
        ms = round((time.perf_counter() - t0) * 1000, 1)
        block = None
        try:
            d = json.loads(body)
            if d.get('result'):
                block = int(d['result'], 16)
        except:
            pass
        return ms, block
    except:
        conns.pop(key, None)
        return -1, None

def probe_loop():
    for name, (h, p, path, use_ssl) in TARGETS.items():
        probe_target(h, p, path, use_ssl)
    while True:
        for name, (h, p, path, use_ssl) in TARGETS.items():
            ms, block = probe_target(h, p, path, use_ssl)
            latest[name] = {'ms': ms, 'block': block}
        latest['timestamp'] = time.time()
        r, s = latest['read']['ms'], latest['sequencer']['ms']
        if r >= 0:
            history['read'].append(r)
            if len(history['read']) > MAX_HISTORY: history['read'].pop(0)
        if s >= 0:
            history['sequencer'].append(s)
            if len(history['sequencer']) > MAX_HISTORY: history['sequencer'].pop(0)
        if r >= 0 and s >= 0:
            history['total'].append(r + 5 + 1 + s)
            if len(history['total']) > MAX_HISTORY: history['total'].pop(0)
        time.sleep(2)


def proxy_bot_json(path, fallback):
    try:
        from urllib import request
        with request.urlopen(f"{BOT_API_BASE}{path}", timeout=2) as resp:
            if resp.status != 200:
                return fallback
            return json.loads(resp.read())
    except Exception:
        return fallback


class Handler(BaseHTTPRequestHandler):
    # Disable reverse DNS lookup that causes hangs with external clients
    def address_string(self):
        return self.client_address[0]
    def do_GET(self):
        if self.path == '/api/latency':
            body = json.dumps({'probes': latest, 'history': history}).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Access-Control-Allow-Origin', '*')
            self.send_header('Content-Length', len(body))
            self.end_headers()
            self.wfile.write(body)
        elif self.path == '/api/chain':
            body = json.dumps(chain_data.get_state()).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Access-Control-Allow-Origin', '*')
            self.send_header('Content-Length', len(body))
            self.end_headers()
            self.wfile.write(body)
        elif self.path == '/api/metrics':
            body = json.dumps(proxy_bot_json('/api/metrics', {
                'status': 'offline',
                'total_liquidations': 0,
                'successful_liquidations': 0,
                'total_profit_usd': 0.0,
                'success_rate': 0.0,
                'avg_latency_ms': 0.0,
                'errors': 0,
                'blocks_processed': 0,
                'positions_scanned': 0,
                'arb_attempts': 0,
                'arb_successes': 0,
                'arb_profit_usd': 0.0,
                'uptime': '0d 0h 0m',
                'uptime_secs': 0
            })).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Access-Control-Allow-Origin', '*')
            self.send_header('Content-Length', len(body))
            self.end_headers()
            self.wfile.write(body)
        elif self.path == '/api/arb':
            body = json.dumps(proxy_bot_json('/api/arb', {
                'scans': 0,
                'opportunities': 0,
                'pairs': [],
                'events': [],
                'spread_history': []
            })).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Access-Control-Allow-Origin', '*')
            self.send_header('Content-Length', len(body))
            self.end_headers()
            self.wfile.write(body)
        elif self.path in ('/', '/index.html'):
            self.send_response(200)
            self.send_header('Content-Type', 'text/html; charset=utf-8')
            self.send_header('Content-Length', len(HTML_BYTES))
            self.end_headers()
            self.wfile.write(HTML_BYTES)
        else:
            self.send_error(404)

    def log_message(self, *args):
        pass


class ThreadedServer(socketserver.ThreadingMixIn, HTTPServer):
    daemon_threads = True
    # Disable reverse DNS lookup — this is what causes hangs on EC2
    def finish_request(self, request, client_address):
        self.RequestHandlerClass(request, client_address, self)


if __name__ == '__main__':
    chain_data.configure(SETTINGS)
    threading.Thread(target=probe_loop, daemon=True).start()
    chain_data.start()
    print("Warming up probes...")
    time.sleep(3)
    print("Chain data fetcher started (updates every 30s)")
    srv = ThreadedServer(('127.0.0.1', SERVER_PORT), Handler)
    print(f"Dashboard: http://127.0.0.1:{SERVER_PORT}")
    srv.serve_forever()
