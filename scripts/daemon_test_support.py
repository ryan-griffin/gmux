"""Wait for the independently running backend during test cleanup."""
import json
from pathlib import Path
import socket
import time


def stop_daemon(path, timeout=8):
    path = Path(path)
    if not path.exists():
        return
    try:
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(6)
            connection.connect(str(path))
            connection.sendall(b'{"method":"quit"}\n')
            result = json.loads(connection.makefile("rb").readline())
            assert "error" not in result, result
    except (OSError, ValueError):
        pass
    deadline = time.monotonic() + timeout
    while path.exists() and time.monotonic() < deadline:
        time.sleep(0.025)
    assert not path.exists(), f"Daemon did not stop at {path}"
