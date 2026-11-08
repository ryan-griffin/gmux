#!/usr/bin/env python3
"""Measure sparse and busy daemon scheduling with matching preserved release binaries."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import socket
import statistics
import subprocess
import threading
import time

from stream_test_support import Client, PROTOCOL_VERSION
from test_support import AppFixture, wait_for


def resources(pid):
    task = Path(f'/proc/{pid}/task/{pid}')
    fields = (task / 'stat').read_text().rsplit(')', 1)[1].split()
    status = dict(line.split(':', 1) for line in (task / 'status').read_text().splitlines()
                  if ':' in line)
    return dict(cpu_ticks=int(fields[11]) + int(fields[12]),
                switches=sum(int(status[key]) for key in
                             ('voluntary_ctxt_switches', 'nonvoluntary_ctxt_switches')),
                rss_kib=int(status['VmRSS'].split()[0]))


def delta(before, after, seconds):
    return dict(owner_cpu_percent=100 * (after['cpu_ticks'] - before['cpu_ticks']) /
                os.sysconf('SC_CLK_TCK') / seconds,
                owner_switches_per_second=(after['switches'] - before['switches']) / seconds,
                rss_kib=after['rss_kib'])


def quantiles(values):
    ordered = sorted(values)
    return dict(median=statistics.median(ordered),
                p95=ordered[min(len(ordered) - 1, int(len(ordered) * .95))], max=max(ordered))


def remote_status(address):
    with socket.create_connection(('127.0.0.1', int(address.rsplit(':', 1)[1])), 6) as peer:
        reader = peer.makefile('rb')
        peer.sendall((json.dumps(dict(version=PROTOCOL_VERSION, channel='control')) + '\n').encode())
        assert 'error' not in json.loads(reader.readline())
        peer.sendall(b'{"method":"status"}\n')
        reply = json.loads(reader.readline())
        assert 'error' not in reply, reply


def run(args):
    profile = args.output.with_suffix('.profile.json').resolve()
    if args.profile:
        profile.unlink(missing_ok=True)
    env = dict(GMUX_OWNER_PROFILE=str(profile)) if args.profile else {}
    with AppFixture(prefix='gmux-scheduling-', env=env) as fixture:
        if not args.profile:
            fixture.env.pop('GMUX_OWNER_PROFILE', None)
        with args.output.with_suffix('.log').open('w') as log:
            process = fixture.start(args.binary, 'serve', stdout=log, stderr=log)
            wait_for(lambda: fixture.rpc('status'))
            panes = [fixture.rpc('new')['pane'] for _ in range(args.panes)]
            probe = fixture.rpc('new')['pane']
            fixture.rpc('remote-access-set', remote_access=dict(enabled=True,
                        listen_address='127.0.0.1:0', authentication=dict(mode='none')))
            address = wait_for(lambda: fixture.rpc('status')['bound_address'])
            clients, threads, received, errors = [], [], [0] * args.clients, []
            stop = threading.Event()

            def drain(index, peer):
                peer.socket.settimeout(.2)
                while not stop.is_set():
                    try:
                        data = peer.socket.recv(262144)
                        if not data:
                            errors.append(f'client {index} disconnected')
                            return
                        received[index] += len(data)
                    except socket.timeout:
                        pass
                    except OSError as error:
                        if not stop.is_set():
                            errors.append(str(error))
                        return

            observer = Client(fixture, probe)
            observer.output('')
            remote = Client(fixture, probe, address=address)
            remote.output('')
            for index in range(args.clients):
                peer = Client(fixture, panes[index % args.active],
                              address=address if index % 2 else None)
                if args.shared_views:
                    peer.send('subscribe', panes=panes[:args.active])
                clients.append(peer)
                thread = threading.Thread(target=drain, args=(index, peer))
                thread.start()
                threads.append(thread)
            # Raw echo measures stream input-to-view, without shell command execution.
            echo = fixture.path / 'echo.py'
            echo.write_text('import os, tty\ntty.setraw(0)\nos.write(1,b"READY")\n'
                            'while True:\n data=os.read(0,4096)\n os.write(1,b"\\r\\n"+data)\n')
            fixture.rpc('send', pane=probe, text=f'python3 {shlex.quote(str(echo))}', paste=True, enter=True)
            observer.output('READY')
            remote.output('READY')
            time.sleep(.4)
            before = resources(process.pid)
            idle_started = time.monotonic()
            time.sleep(1.5)
            idle_attached = delta(before, resources(process.pid), time.monotonic() - idle_started)
            writers = []
            for index, pane in enumerate(panes[:args.active]):
                counter = fixture.path / f'counter-{index}'
                program = fixture.path / f'writer-{index}.py'
                program.write_text(
                    'import os,time\n'
                    f'counter={str(counter)!r}\n'
                    f'batch=(b"\\x1b[32mrow_\\xe7\\x95\\x8c_e\\xcc\\x81_"+b"x"*80+b"\\x1b[0m\\r\\n")*{args.batch_lines}\n'
                    'total=0\nlast=0\n'
                    'while True:\n'
                    ' total+=os.write(1,batch)\n'
                    ' now=time.monotonic()\n'
                    ' if now-last>.05:\n'
                    '  with open(counter,"w") as f: f.write(str(total))\n'
                    '  last=now\n'
                    f' time.sleep({args.interval})\n')
                fixture.rpc('send', pane=pane, text=f'python3 {shlex.quote(str(program))}', paste=True, enter=True)
                writers.append(counter)
            wait_for(lambda: all(path.exists() for path in writers))
            time.sleep(.5)

            def counts():
                # Writers replace a tiny file; retry if caught between truncate/write.
                return [wait_for(lambda path=path: path.read_text()) for path in writers]

            start_counts = list(map(int, counts()))
            start_bytes = received.copy()
            before = resources(process.pid)
            started = time.monotonic()
            local_control, tcp_control, echo_latencies = [], [], []
            for index in range(args.samples):
                at = time.monotonic()
                fixture.rpc('status')
                local_control.append((time.monotonic() - at) * 1000)
                at = time.monotonic()
                remote_status(address)
                tcp_control.append((time.monotonic() - at) * 1000)
                marker = f'E{index:05d}'
                reader = observer if index % 2 == 0 else remote
                at = time.monotonic()
                reader.send('input', bytes=list(marker.encode()))
                reader.output(marker)
                echo_latencies.append((time.monotonic() - at) * 1000)
                time.sleep(args.seconds / args.samples)
            elapsed = time.monotonic() - started
            busy = delta(before, resources(process.pid), elapsed)
            end_counts = list(map(int, counts()))
            busy.update(seconds=elapsed,
                        writer_mib_per_second=[(b-a)/elapsed/2**20 for a,b in zip(start_counts,end_counts)],
                        reader_bytes=[b-a for a,b in zip(start_bytes,received)],
                        local_control_ms=quantiles(local_control), tcp_control_ms=quantiles(tcp_control),
                        input_to_view_ms=quantiles(echo_latencies), status=fixture.rpc('status'))
            assert not errors, errors
            assert all(b > a for a,b in zip(start_counts,end_counts)), 'Writer starvation'
            assert all(b > a for a,b in zip(start_bytes,received)), 'Reader starvation'
            # Stop output before measuring detached idle, preserving the topology.
            for pane in panes[:args.active]:
                fixture.rpc('send', pane=pane, text='\x03')
            stop.set()
            for thread in threads:
                thread.join(timeout=2)
                assert not thread.is_alive()
            for peer in [*clients, observer, remote]:
                peer.close()
            wait_for(lambda: fixture.rpc('status')['local_clients'] == 0 and
                     fixture.rpc('status')['remote_clients'] == 0)
            # Allow output drains and the existing compression deadline to settle.
            time.sleep(3)
            before = resources(process.pid)
            idle_started = time.monotonic()
            time.sleep(1.5)
            idle_detached = delta(before, resources(process.pid), time.monotonic() - idle_started)
            fixture.stop()
    return dict(binary=str(args.binary), sha256=hashlib.sha256(args.binary.read_bytes()).hexdigest(),
                config=vars(args) | dict(binary=str(args.binary), output=str(args.output)),
                idle_attached=idle_attached, busy=busy, idle_detached=idle_detached,
                profile=json.loads(profile.read_text()) if args.profile else None)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--panes', type=int, default=128)
    parser.add_argument('--active', type=int, default=2)
    parser.add_argument('--clients', type=int, default=8)
    parser.add_argument('--seconds', type=float, default=5)
    parser.add_argument('--samples', type=int, default=100)
    parser.add_argument('--interval', type=float, default=.005)
    parser.add_argument('--batch-lines', type=int, default=100)
    parser.add_argument('--profile', action='store_true')
    parser.add_argument('--shared-views', action='store_true', help='Every reader views every active pane')
    args = parser.parse_args()
    assert 0 < args.active <= args.panes and args.clients > 0 and args.samples > 0
    assert args.seconds > 0 and args.interval >= 0 and args.batch_lines > 0
    args.binary = args.binary.resolve()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(run(args), indent=2) + '\n')
    print(args.output)
