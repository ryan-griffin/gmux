#!/usr/bin/env python3
"""Check aggregate daemon pressure, viewport recovery, and stream admission."""
import socket
import subprocess
import sys
from pathlib import Path

from cargo_test_support import cargo_binary
from stream_test_support import Client
from test_support import AppFixture, scenario, wait_for

binary = cargo_binary('gmux')
root = Path(__file__).resolve().parent.parent
with scenario('slow local streams preserve local and TCP interaction; evicted views resynchronize'):
    subprocess.run([sys.executable, str(root / 'scripts/daemon-resource-benchmark.py'),
                    '--binary', str(binary), '--verify', '--output',
                    str(root / 'artifacts/daemon-resource-smoke.json')], check=True, timeout=90)

with scenario('idle streams share admission and release slots without blocking control calls'), AppFixture() as fixture:
    fixture.start(binary, 'serve', stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    status = wait_for(lambda: fixture.rpc('status'))
    limit = status['frontend_resources']['stream_limit']
    peers = []
    recovered = None
    try:
        for _ in range(limit):
            peer = socket.socket(socket.AF_UNIX)
            peer.settimeout(3)
            peer.connect(fixture.env['GMUX_SOCKET'] + '.stream')
            peers.append(peer)
        wait_for(lambda: fixture.rpc('status')['frontend_resources']['streams'] == limit)
        with socket.socket(socket.AF_UNIX) as rejected:
            rejected.settimeout(3)
            rejected.connect(fixture.env['GMUX_SOCKET'] + '.stream')
            assert rejected.recv(1) == b'', 'Excess frontend was admitted'
        peers.pop().close()
        wait_for(lambda: fixture.rpc('status')['frontend_resources']['streams'] == limit - 1)
        pane = fixture.rpc('new')['pane']
        recovered = Client(fixture, pane)
        recovered.send('input', bytes=list(b"printf 'ADMISSION_RECOVERED\\n'\n"))
        recovered.output('ADMISSION_RECOVERED')
        assert fixture.rpc('status')['local_clients'] == 1
    finally:
        if recovered is not None:
            recovered.close()
        for peer in peers:
            peer.close()
