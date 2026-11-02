#!/usr/bin/env python3
"""Measure daemon output/cache storage and input latency with slow stream peers."""
import argparse
import json
from pathlib import Path
import shlex
import statistics
import subprocess
import time

from cargo_test_support import cargo_binary
from stream_test_support import Client
from test_support import AppFixture, wait_for


def memory(pid):
    fields = dict(line.split(':', 1) for line in Path(f'/proc/{pid}/status').read_text().splitlines()
                  if ':' in line)
    return {name: int(fields[name].split()[0]) for name in ('VmRSS', 'VmHWM')}


def sample(fixture, process):
    status = fixture.rpc('status')
    return dict(memory_kib=memory(process.pid), resources=status['frontend_resources'],
                clients=status['local_clients'] + status['remote_clients'], snapshots=status['snapshots'], sent_rows=status['sent_rows'])


def run(args, *, caches=False):
    with AppFixture(prefix='gmux-daemon-budget-') as fixture:
        binary = args.binary or cargo_binary('gmux', profile='release')
        process = fixture.start(binary, 'serve', stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        wait_for(lambda: fixture.rpc('status'))
        pane = fixture.rpc('new')['pane']
        interactive = fixture.rpc('new')['pane']
        peers = []
        observer = Client(fixture, interactive)
        remote = None
        try:
            observer.output('')
            observer.send('focused', active=True)
            if not caches:
                fixture.rpc('remote-access-set', remote_access=dict(enabled=True,
                    listen_address='127.0.0.1:0', authentication=dict(mode='none')))
                address = wait_for(lambda: fixture.rpc('status')['bound_address'])
                remote = Client(fixture, interactive, address=address)
                remote.output('')
            else:
                fixture.rpc('send', pane=pane,
                            text="for i in $(seq 0 199); do printf 'cache-row-%03d\\n' \"$i\"; done; "
                                 "printf '\\033]8;;https://example.com/cache\\aCACHE_界_é\\033]8;;\\a\\n'",
                            paste=True, enter=True)
                wait_for(lambda: 'CACHE_' in fixture.rpc('read', pane=pane)['text'])
            idle = sample(fixture, process)
            for _ in range(24 if caches else 12):
                peer = Client(fixture, pane)
                peers.append(peer)
                peer.send('resize', cols=512 if caches else 160, rows=128 if caches else 50)
                initial = peer.output('CACHE_' if caches else '')
                if caches and len(peers) == 1:
                    peer.send('scroll-to', row=0, serial=7001)
                    initial = peer.until(lambda h: h['scroll_serial'] == 7001 and h['frame'] is not None)
                    assert peer.frame['scrollback']['total'] > peer.frame['rows']
                    peer.send('select', start=[0, 0], end=[4, 0], serial=1)
                    peer.until(lambda h: h['selection_serial'] == 1)
                    peer.send('copy')
                    selected = peer.until(lambda h: h['copy'] is not None)['copy']['content']['text']
                    initial_generation = initial['generation']
            # These readers have all applied a full initial viewport. Thereafter
            # one reads 4 KiB per sample, and the others stop reading entirely.
            # No terminal history is needed to produce the pressure.
            program = fixture.path / 'output.py'
            program.write_text(
                'import os, time\n'
                f'end = time.monotonic() + {args.duration + 2}\n'
                'i = 0\n'
                'while time.monotonic() < end:\n'
                "    os.write(1, b'\\x1b[H' + bytes([65 + i % 26]) * 7999)\n"
                '    i += 1\n'
                '    time.sleep(.016)\n')
            attached = sample(fixture, process)
            if not caches:
                fixture.rpc('send', pane=pane, text=f'python3 {shlex.quote(str(program))}', paste=True, enter=True)
            controls, echoes, samples = [], [], []
            start = time.monotonic()
            index = 0
            while time.monotonic() - start < (1 if caches else args.duration):
                at = time.monotonic()
                fixture.rpc('status')
                controls.append((time.monotonic() - at) * 1000)
                marker = f'BUDGET_ECHO_{index}'
                at = time.monotonic()
                reader = remote if remote is not None and index % 2 else observer
                reader.send('input', bytes=list(f"printf '{marker}\\n'\n".encode()))
                reader.output(marker)
                echoes.append((time.monotonic() - at) * 1000)
                samples.append(sample(fixture, process))
                if not caches:
                    peers[0].socket.setblocking(False)
                    try:
                        peers[0].socket.recv(4096)
                    except BlockingIOError:
                        pass
                    peers[0].socket.settimeout(6)
                index += 1
                time.sleep(.1)
            pressure = sample(fixture, process)
            if caches:
                # The oldest unfocused screen was evicted. Selection belongs to
                # its engine view and must still copy the same bytes.
                peer = peers[0]
                if args.verify:
                    # A zero scroll preserves the existing tracked viewport.
                    peer.send('scroll', delta=0, bottom=False)
                    resync = peer.until(lambda h: h['frame'] is not None and h['generation'] > initial_generation)
                else:
                    peer.send('scroll-to', row=0, serial=9001)
                    resync = peer.until(lambda h: h['scroll_serial'] == 9001 and h['frame'] is not None)
                if args.verify:
                    assert resync['generation'] > initial_generation, resync
                    assert len(resync['rows']) == 128, resync['rows']
                    assert peer.frame['scrollback']['offset'] == 0
                peer.send('copy')
                assert peer.until(lambda h: h['copy'] is not None)['copy']['content']['text'] == selected
                peer.send('scroll', bottom=True)
                peer.output('CACHE_')
                assert peer.links['spans'], peer.links
                # Eviction must also permit an empty hyperlink map to clear the
                # client's previous links after the terminal replaces the row.
                fixture.rpc('send', pane=pane,
                            text="printf '\\033[2J\\033[HCACHE_PLAIN_界_é\\n'", paste=True, enter=True)
                wait_for(lambda: 'CACHE_PLAIN_' in fixture.rpc('read', pane=pane)['text'])
                peer.send('scroll', bottom=True)
                peer.output('CACHE_PLAIN_')
                assert 'CACHE_PLAIN_界_é' in peer.text(), peer.text()
                assert not peer.links['spans'], peer.links
            for peer in peers:
                peer.close()
            if remote is not None:
                remote.close()
            wait_for(lambda: fixture.rpc('status')['local_clients'] == 1)
            released = sample(fixture, process)
            if args.verify:
                for state in [idle, attached, pressure, released, *samples]:
                    resources = state['resources']
                    assert resources['output_capacity'] <= resources['output_limit'], resources
                    assert resources['screen_cache_bytes'] <= resources['screen_cache_limit'], resources
                assert max(controls + echoes) < 250, (controls, echoes)
                assert released['resources']['screen_cache_bytes'] <= idle['resources']['screen_cache_bytes']
                if not caches:
                    assert pressure['clients'] < attached['clients'], 'No slow consumer was disconnected'
            samples += [attached, pressure]
            result = dict(workload='view-caches' if caches else 'slow-output', duration=1 if caches else args.duration,
                          idle=idle, attached=attached, pressure=pressure, released=released,
                          peak_output_capacity=max(s['resources']['output_capacity'] for s in samples),
                          peak_screen_cache_bytes=max(s['resources']['screen_cache_bytes'] for s in samples),
                          control_ms=dict(median=statistics.median(controls), maximum=max(controls)),
                          input_to_view_ms=dict(median=statistics.median(echoes), maximum=max(echoes)))
            print(json.dumps(result), flush=True)
            return result
        finally:
            observer.close()
            if remote is not None:
                remote.close()
            for peer in peers:
                peer.close()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path)
    parser.add_argument('--verify', action='store_true', help='Assert budgets, pressure eviction and viewport recovery')
    parser.add_argument('--duration', type=float, default=8)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.duration < 1:
        parser.error('--duration must be at least one second')
    results = [run(args), run(args, caches=True)]
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(results, indent=2) + '\n')
