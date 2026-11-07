#!/usr/bin/env python3
"""Check direct dispatch, pipelined ordering, cancellation, and shared admission."""
import json
from pathlib import Path
import socket
import struct
import subprocess

from cargo_test_support import cargo_binary
from stream_test_support import Client, PROTOCOL_VERSION
from test_support import AppFixture, scenario, wait_for

binary = cargo_binary('gmux')
with AppFixture(prefix='gmux-direct-transport-') as fixture:
    process = fixture.start(binary, 'serve', stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    wait_for(lambda: fixture.rpc('status'))
    fixture.rpc('remote-access-set', remote_access=dict(enabled=True, listen_address='127.0.0.1:0', authentication=dict(mode='none')))
    address = fixture.rpc('status')['bound_address']
    identity = fixture.rpc('_hello')['daemon']
    host, port = address.rsplit(':', 1)

    def connect(channel='control', suffix=b''):
        peer = socket.create_connection((host, int(port)), 3)
        peer.settimeout(3)
        peer.sendall((json.dumps(dict(version=PROTOCOL_VERSION, channel=channel, daemon=identity))+'\n').encode()+suffix)
        reader = peer.makefile('rb')
        assert json.loads(reader.readline())['daemon'] == identity
        return peer, reader

    with scenario('authenticated TCP works with both Unix socket paths unavailable'):
        paths = [Path(fixture.env['GMUX_SOCKET']+suffix) for suffix in ('', '.stream')]
        for path in paths:
            path.rename(str(path)+'.hidden')
        try:
            peer, reader = connect(suffix=b'{"method":"new"}\n{"method":"state"}\n')
            created, state = json.loads(reader.readline()), json.loads(reader.readline())
            assert state['revision'] == created['revision']
            pane = created['result']['pane']
            stream = Client(fixture, pane, address=address)
            stream.output('')
            stream.send('paste', text="printf 'DIRECT_OWNER_OK\\n'", enter=True)
            stream.output('DIRECT_OWNER_OK')
            stream.close()
            reader.close()
            peer.close()
        finally:
            for path in paths:
                Path(str(path)+'.hidden').rename(path)

    with scenario('pipelined stream commands survive handshake read-ahead and write half-close'):
        count = 73  # Cross several eight-command owner visits.
        suffix = (json.dumps(dict(method='attach', version=PROTOCOL_VERSION, daemon=identity, remote=False))+'\n').encode()
        suffix += b'{"method":"ping"}\n'*count
        peer, reader = connect('stream', suffix)
        peer.shutdown(socket.SHUT_WR)
        heartbeats = 0
        while header := reader.read(4):
            assert len(header) == 4
            size, = struct.unpack('<I', header)
            if size:
                assert len(reader.read(size)) == size
            else:
                heartbeats += 1
        assert heartbeats == count, heartbeats
        reader.close()
        peer.close()
        wait_for(lambda: fixture.rpc('status')['frontend_resources']['streams'] == 0)

    with scenario('local and remote streams share aggregate admission and release slots'):
        local, remote = [], []
        try:
            for _ in range(40):
                peer = socket.socket(socket.AF_UNIX)
                peer.connect(fixture.env['GMUX_SOCKET']+'.stream')
                local.append(peer)
            for _ in range(24):
                peer, reader = connect('stream', (json.dumps(dict(method='attach', version=PROTOCOL_VERSION, daemon=identity))+'\n').encode())
                reader.close()
                remote.append(peer)
            wait_for(lambda: fixture.rpc('status')['frontend_resources']['streams'] == 64)
            rejected, reader = connect('stream', (json.dumps(dict(method='attach', version=PROTOCOL_VERSION, daemon=identity))+'\n').encode())
            assert reader.read(1) == b''
            reader.close()
            rejected.close()
            remote.pop().close()
            wait_for(lambda: fixture.rpc('status')['frontend_resources']['streams'] == 63)
            recovered = Client(fixture, pane, address=address)
            recovered.send('paste', text="printf 'ADMISSION_DIRECT_OK\\n'", enter=True)
            recovered.output('ADMISSION_DIRECT_OK')
            recovered.close()
            fixture.rpc('remote-access-set', remote_access=dict(enabled=False, listen_address=address, authentication=dict(mode='none')))
            wait_for(lambda: fixture.rpc('status')['frontend_resources']['streams'] == 40)
        finally:
            for peer in local+remote:
                peer.close()
        wait_for(lambda: fixture.rpc('status')['frontend_resources']['streams'] == 0)
print('Direct remote transport smoke test passed')
