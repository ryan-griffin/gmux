#!/usr/bin/env python3
"""Check direct dispatch, pipelined ordering, cancellation, and shared admission."""
from contextlib import ExitStack, closing
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
        def frontend_resources():
            return fixture.rpc('status')['frontend_resources']

        limit = frontend_resources()['stream_limit']
        remote_count = min(24, limit // 2)
        local_count = limit - remote_count
        unix_path = fixture.env['GMUX_SOCKET'] + '.stream'
        attach = (json.dumps(dict(method='attach', version=PROTOCOL_VERSION, daemon=identity)) + '\n').encode()
        local, remote = [], []
        try:
            for _ in range(local_count):
                peer = socket.socket(socket.AF_UNIX)
                peer.settimeout(3)
                peer.connect(unix_path)
                local.append(peer)
            for _ in range(remote_count):
                peer, reader = connect('stream', attach)
                reader.close()
                remote.append(peer)
            wait_for(lambda: frontend_resources()['streams'] == limit)
            with socket.socket(socket.AF_UNIX) as rejected:
                rejected.settimeout(3)
                rejected.connect(unix_path)
                assert rejected.recv(1) == b'', 'Excess Unix stream was admitted'
            rejected, reader = connect('stream', attach)
            with rejected, reader:
                assert reader.read(1) == b'', 'Excess TCP stream was admitted'

            with ExitStack() as recovered:
                for peers, endpoint, transport in ((local, None, 'UNIX'), (remote, address, 'TCP')):
                    peers.pop().close()
                    wait_for(lambda: frontend_resources()['streams'] == limit - 1)
                    client = recovered.enter_context(closing(Client(fixture, pane, address=endpoint)))
                    client.send('paste', text=f"printf 'ADMISSION_%s_OK\\n' {transport}", enter=True)
                    client.output(f'ADMISSION_{transport}_OK')
                assert fixture.rpc('status')['local_clients'] == 1
                fixture.rpc('remote-access-set', remote_access=dict(enabled=False, listen_address=address, authentication=dict(mode='none')))
                wait_for(lambda: frontend_resources()['streams'] == local_count)
        finally:
            for peer in local + remote:
                peer.close()
        wait_for(lambda: frontend_resources()['streams'] == 0)
print('Direct remote transport smoke test passed')
