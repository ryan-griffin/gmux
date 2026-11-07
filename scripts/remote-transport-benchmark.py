#!/usr/bin/env python3
"""Measure mixed Unix/TCP interaction and daemon resources on Linux loopback."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shlex
import socket
import statistics
import struct
import subprocess
import threading
import time

from stream_test_support import Client, PROTOCOL_VERSION
from test_support import AppFixture, wait_for


def distribution(values):
    ordered = sorted(values)
    return dict(samples=len(values), median=statistics.median(values),
                p95=ordered[min(len(values)-1, int(len(values)*.95))],
                p99=ordered[min(len(values)-1, int(len(values)*.99))], maximum=max(values))


def resources(pid):
    status = dict(line.split(':', 1) for line in Path(f'/proc/{pid}/status').read_text().splitlines())
    switches = 0
    for task in Path(f'/proc/{pid}/task').iterdir():
        try:
            fields = dict(line.split(':', 1) for line in (task/'status').read_text().splitlines())
            switches += sum(int(fields[key]) for key in ('voluntary_ctxt_switches', 'nonvoluntary_ctxt_switches'))
        except FileNotFoundError:
            pass
    stat = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
    return dict(cpu_seconds=(int(stat[11])+int(stat[12]))/os.sysconf('SC_CLK_TCK'),
                context_switches=switches, threads=int(status['Threads']),
                fds=len(list(Path(f'/proc/{pid}/fd').iterdir())),
                rss_kib=int(status['VmRSS'].split()[0]), hwm_kib=int(status['VmHWM'].split()[0]))


def heartbeat(client):
    consumed = 0
    while True:
        size, = struct.unpack('<I', client.exact(4))
        consumed += size + 4
        if size == 0:
            return consumed
        client.exact(size)


def run(args, pressure):
    with AppFixture(prefix='gmux-transport-bench-') as fixture:
        process = fixture.start(args.binary, 'serve', stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        wait_for(lambda: fixture.rpc('status'))
        fixture.rpc('remote-access-set', remote_access=dict(enabled=True, listen_address='127.0.0.1:0', authentication=dict(mode='none')))
        address = fixture.rpc('status')['bound_address']
        pane = fixture.rpc('new')['pane']
        noisy = fixture.rpc('new')['pane']
        local = Client(fixture, pane)
        remote = Client(fixture, pane, address=address)
        peers = []
        try:
            for client in (local, remote):
                client.output('')
            idle = resources(process.pid)
            # Four additional TCP frontends stay active; twelve more stop reading
            # or drain only a small amount. All share the owner's fair work queues.
            for index in range(16 if pressure else 4):
                client = Client(fixture, noisy, address=address)
                client.send('resize', cols=160, rows=50)
                client.output('')
                peers.append(client)
            stopping = threading.Event()
            drained_bytes = [0] * 4
            def drain(client, index):
                try:
                    while not stopping.is_set():
                        client.send('ping')
                        drained_bytes[index] += heartbeat(client)
                        stopping.wait(.02)
                except (OSError, AssertionError):
                    pass
            workers = [threading.Thread(target=drain, args=(p, index)) for index, p in enumerate(peers[:4])]
            for worker in workers:
                worker.start()
            if pressure:
                program = fixture.path/'output.py'
                program.write_text("import os,time\nfor i in range(10000):\n os.write(1,b'\\x1b[H'+bytes([65+i%26])*7999)\n time.sleep(.016)\n")
                fixture.rpc('send', pane=noisy, text=f'python3 {shlex.quote(str(program))}', enter=True)
            initial_status = fixture.rpc('status')
            before = resources(process.pid)
            start = time.monotonic()
            results = {}
            for name, client in [('unix', local), ('tcp', remote)]:
                pings, echoes = [], []
                for index in range(args.samples):
                    at = time.perf_counter()
                    client.send('ping')
                    heartbeat(client)
                    pings.append((time.perf_counter()-at)*1000)
                    marker = f'TRANSPORT_{name}_{index}'
                    at = time.perf_counter()
                    client.send('input', bytes=list(f"printf '{marker}\\n'\n".encode()))
                    client.output(marker)
                    echoes.append((time.perf_counter()-at)*1000)
                    if pressure:
                        peers[4].socket.setblocking(False)
                        try:
                            peers[4].socket.recv(4096)
                        except BlockingIOError:
                            pass
                        peers[4].socket.settimeout(6)
                at = time.perf_counter()
                count = 2000
                client.socket.sendall(b'{"method":"ping"}\n' * count)
                for _ in range(count):
                    heartbeat(client)
                elapsed = time.perf_counter()-at
                results[name] = dict(ping_ms=distribution(pings), input_to_view_ms=distribution(echoes),
                                     ordered_ping_replies_per_second=count/elapsed)
            after = resources(process.pid)
            elapsed = time.monotonic()-start
            final_status = fixture.rpc('status')
            stopping.set()
            for worker in workers:
                worker.join(timeout=7)
            # Control timings use persistent TCP and one-shot Unix endpoints.
            # They are outside the CPU/context-switch interval, which otherwise
            # has stable threads and no per-request Unix handler churn.
            host, port = address.rsplit(':', 1)
            control = socket.create_connection((host, int(port)), 6)
            control.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            control.sendall((json.dumps(dict(version=PROTOCOL_VERSION, channel='control'))+'\n').encode())
            reader = control.makefile('rb')
            json.loads(reader.readline())
            control_times = {}
            for name in ('unix', 'tcp'):
                timings = []
                for _ in range(args.samples):
                    at = time.perf_counter()
                    if name == 'unix':
                        fixture.rpc('status')
                    else:
                        control.sendall(b'{"method":"status"}\n')
                        assert 'error' not in json.loads(reader.readline())
                    timings.append((time.perf_counter()-at)*1000)
                control_times[name] = distribution(timings)
            reader.close()
            control.close()
            results.update(control_roundtrip_ms=control_times,
                           drained_stream_bytes_per_second=sum(drained_bytes)/elapsed,
                           snapshots_per_second=(final_status['snapshots']-initial_status['snapshots'])/elapsed,
                           sent_rows_per_second=(final_status['sent_rows']-initial_status['sent_rows'])/elapsed,
                           pressure=pressure, idle=idle, attached=before, final=after,
                           wall_seconds=elapsed,
                           cpu_seconds=after['cpu_seconds']-before['cpu_seconds'],
                           context_switches=after['context_switches']-before['context_switches'])
            return results
        finally:
            local.close()
            remote.close()
            for peer in peers:
                peer.close()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--samples', type=int, default=100)
    parser.add_argument('--runs', type=int, default=3)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.samples < 1 or args.runs < 1:
        parser.error('--samples and --runs must be positive')
    result = dict(platform=platform.platform(), cpu_affinity=sorted(os.sched_getaffinity(0)),
                  protocol_version=PROTOCOL_VERSION, samples_per_transport=args.samples,
                  binary=str(args.binary), sha256=hashlib.sha256(args.binary.read_bytes()).hexdigest(),
                  runs=[run(args, pressure) for _ in range(args.runs) for pressure in (False, True)])
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2)+'\n')
    print(args.output)
