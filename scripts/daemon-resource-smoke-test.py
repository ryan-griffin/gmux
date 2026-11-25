#!/usr/bin/env python3
"""Check aggregate daemon pressure and viewport recovery."""
import subprocess
import sys
from pathlib import Path

from cargo_test_support import cargo_binary
from test_support import scenario

binary = cargo_binary('gmux')
root = Path(__file__).resolve().parent.parent
with scenario('slow local streams preserve local and TCP interaction; evicted views resynchronize'):
    subprocess.run([sys.executable, str(root / 'scripts/daemon-resource-benchmark.py'),
                    '--binary', str(binary), '--verify', '--output',
                    str(root / 'artifacts/daemon-resource-smoke.json')], check=True, timeout=90)
