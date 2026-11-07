#!/usr/bin/env python3
"""Measure complete searches, rescans, independent views, holds, and teardown."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import shlex
import subprocess
import time

import stream_test_support
from stream_test_support import Client
from test_support import AppFixture, wait_for


class SearchClient(Client):
    find_state = None

    def until(self, predicate):
        def observe(header):
            if header.get('find') is not None:
                self.find_state = header['find']
            return predicate(header)
        return super().until(observe)

    def settled_output(self, marker):
        self.output(marker)
        if self.find_state is not None and self.find_state['pending']:
            self.until(lambda h: h.get('find') is not None and not h['find']['pending'])
        return self.find_state


def run(args):
    with AppFixture(prefix='gmux-view-search-') as fixture:
        process = fixture.start(args.binary, 'serve', stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        wait_for(lambda: fixture.rpc('status'))
        pane = fixture.rpc('new')['pane']
        interactive = fixture.rpc('new')['pane']
        observer = Client(fixture, interactive)
        observer.output('')
        peers = []
        report = dict(samples={}, searches=[], controls_ms=[], input_ms=[], navigation_ms=[], peak_resources={}, peak_rss_kib=0)
        started = time.monotonic()
        def cpu_seconds():
            fields = Path(f"/proc/{process.pid}/stat").read_text().split()
            return (int(fields[13]) + int(fields[14])) / os.sysconf("SC_CLK_TCK")
        initial_cpu = cpu_seconds()

        def sample(name):
            status = fixture.rpc('status')
            fields = dict(line.split(':', 1) for line in Path(f'/proc/{process.pid}/status').read_text().splitlines() if ':' in line)
            report['samples'][name] = dict(resources=status['frontend_resources'],
                rss_kib=int(fields['VmRSS'].split()[0]), hwm_kib=int(fields['VmHWM'].split()[0]))
            report['peak_rss_kib'] = max(report['peak_rss_kib'], int(fields['VmRSS'].split()[0]))
            for key, value in status['frontend_resources'].items():
                report['peak_resources'][key] = max(report['peak_resources'].get(key, 0), value)
            resources = status['frontend_resources']
            if 'search_limit' in resources:
                assert resources['search_bytes'] <= resources['search_limit']
                assert resources['view_native_reserved'] <= resources['view_native_limit']
                assert resources['views'] <= resources['view_limit']

        def search(peer, query):
            at = time.monotonic()
            peer.send('find', query=query)
            def completed():
                state = peer.until(lambda h: h.get('find') is not None and not h['find']['pending']
                    and (h['find']['total'] or h['find'].get('resource_limited')))['find']
                return dict(ms=(time.monotonic()-at)*1000, state=state | dict(highlights=[]))
            with ThreadPoolExecutor(max_workers=1) as worker:
                future = worker.submit(completed)
                while not future.done():
                    start = time.monotonic()
                    fixture.rpc('status')
                    report['controls_ms'].append((time.monotonic() - start) * 1000)
                    marker = f'INPUT_{len(report["input_ms"])}'
                    start = time.monotonic()
                    observer.send('input', bytes=list(f"printf '{marker}\\n'\n".encode()))
                    observer.output(marker)
                    report['input_ms'].append((time.monotonic() - start) * 1000)
                    sample('scanning')
                    time.sleep(.01)
                report['searches'].append(future.result())

        sample('idle')
        writer = fixture.path / 'dense.py'
        line = b"x" * 152 + b"needle" + b"x" if args.case == 'sparse' else b"a" * 159
        writer.write_text(f'import os\nos.write(1, {line + bytes([13,10])!r} * {args.rows})\nprint("DENSE_READY")\n')
        fixture.rpc('send', pane=pane, text=f'python3 {shlex.quote(str(writer))}', paste=True, enter=True)
        wait_for(lambda: 'DENSE_READY' in fixture.rpc('read', pane=pane)['text'])
        for index in range(args.views):
            peer = SearchClient(fixture, pane)
            peers.append(peer)
            peer.send('resize', cols=160, rows=50)
            peer.output('DENSE_READY')
            search(peer, 'needle' if args.case == 'sparse' else 'a')
            sample(f'search_{index+1}')
            if peer.find_state['total'] > 1 and not peer.find_state.get('resource_limited'):
                total = peer.find_state['total']
                for previous in [True] + [False, True] * args.navigation_rounds:
                    expected = total if previous else 1
                    start = time.monotonic()
                    peer.send('find-advance', previous=previous)
                    state = peer.until(lambda h: h.get('find') is not None and h['find']['current'] == expected)['find']
                    report['navigation_ms'].append((time.monotonic()-start)*1000)
                    assert state['total'] == total
        # A second scan keeps the old complete index while building the new one.
        rescan_started = time.monotonic()
        fixture.rpc('send', pane=pane, text="printf 'RESCAN_READY\\n'", paste=True, enter=True)
        peers[0].output('RESCAN_READY')
        sample('rescan')
        if peers[0].find_state['pending']:
            peers[0].until(lambda h: h.get('find') is not None and not h['find']['pending'])
        report['rescan_ms'] = (time.monotonic() - rescan_started) * 1000
        sample('rescan_complete')
        fixture.rpc('send', pane=pane, text="printf '\\033[?2026hHOLD_READY'; sleep .5; printf '\\033[?2026l'", paste=True, enter=True)
        time.sleep(.15)
        sample('hold')
        report['after_hold'] = []
        for peer in peers:
            state = peer.settled_output('HOLD_READY')
            report['after_hold'].append(state | dict(highlights=[]))
        sample('settled')
        for peer in peers:
            peer.send('subscribe', panes=[])
        time.sleep(.1)
        sample('hidden')
        if 'search_bytes' in report['samples']['hidden']['resources']:
            assert report['samples']['hidden']['resources']['search_bytes'] == report['samples']['settled']['resources']['search_bytes']
        for peer in peers:
            peer.send('find', query='')
            peer.until(lambda h: h.get('find') is not None and not h['find']['pending']
                       and h['find']['total'] == 0 and not h['find'].get('resource_limited'))
        sample('cancelled')
        fixture.rpc('close', pane=pane)
        wait_for(lambda: fixture.rpc('status')['frontend_resources'].get('views', 1) == 1)
        sample('pane_closed')
        for peer in peers:
            peer.socket.close()
        wait_for(lambda: fixture.rpc('status')['frontend_resources']['streams'] == 1)
        sample('disconnected')
        observer.socket.close()
        wait_for(lambda: fixture.rpc('status')['frontend_resources']['streams'] == 0)
        sample('closed')
        if 'views' in report['samples']['closed']['resources']:
            final = report['samples']['closed']['resources']
            assert final['views'] == final['search_bytes'] == final['view_native_reserved'] == final['view_bytes'] == 0
        if args.assert_complete:
            assert all(s['state']['total'] > 0 and not s['state'].get('resource_limited') for s in report['searches'])
            assert all(s['total'] > 0 and not s.get('resource_limited') for s in report['after_hold'])
        report['cpu_seconds'] = cpu_seconds() - initial_cpu
        report['wall_seconds'] = time.monotonic() - started
        return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True)
    parser.add_argument('--case', choices=['dense', 'sparse'], default='dense')
    parser.add_argument('--navigation-rounds', type=int, default=1)
    parser.add_argument('--assert-complete', action='store_true')
    parser.add_argument('--rows', type=int, default=12000)
    parser.add_argument('--views', type=int, default=4)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--protocol-version', type=int, default=stream_test_support.PROTOCOL_VERSION)
    args = parser.parse_args()
    if args.rows < 1 or args.views < 1 or args.navigation_rounds < 0:
        parser.error('rows and views must be positive; navigation-rounds must be nonnegative')
    stream_test_support.PROTOCOL_VERSION = args.protocol_version
    report = run(args)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + '\n')
    print(args.output)


if __name__ == '__main__':
    main()
