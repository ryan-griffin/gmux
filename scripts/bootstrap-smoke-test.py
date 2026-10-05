#!/usr/bin/env python3
"""Check dependency download recovery against a private HTTP server."""
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import threading
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("bootstrap_ghostty", Path(__file__).with_name("bootstrap-ghostty.py"))
bootstrap = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bootstrap)

payload = b"complete dependency archive"
requests = []
os.environ["NO_PROXY"] = os.environ["no_proxy"] = "127.0.0.1"


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        requests.append(self.path)
        self.send_response(200)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        if self.path == "/interrupted" and requests.count(self.path) == 1:
            self.wfile.write(payload[:4])
            self.close_connection = True
        else:
            self.wfile.write(payload)

    def log_message(self, *_):
        pass


with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    try:
        with tempfile.TemporaryDirectory(prefix="gmux-bootstrap-test-") as directory:
            directory = Path(directory)
            destination = directory / "dep.tar.gz"
            url = f"http://127.0.0.1:{server.server_port}/interrupted"
            try:
                bootstrap.download(url, destination)
            except subprocess.CalledProcessError:
                pass
            else:
                raise AssertionError("Truncated HTTP response should fail")
            assert list(directory.iterdir()) == [], "Failed downloads must not enter the cache or leave temporary files"
            bootstrap.download(url, destination)
            assert destination.read_bytes() == payload
            bootstrap.download(url, destination)
            assert requests.count("/interrupted") == 2, "Complete cached downloads should be reused"
            checksum = hashlib.sha256(payload).hexdigest()
            bootstrap.download(url, destination, checksum)
            destination.write_bytes(b"damaged cached archive")
            try:
                bootstrap.download(url, destination, checksum)
            except SystemExit as error:
                assert "Checksum mismatch" in str(error)
            else:
                raise AssertionError("Damaged cached archive should fail verification")
            assert not destination.exists()
            bootstrap.download(url, destination, checksum)
            assert destination.read_bytes() == payload
            destination.unlink()
            try:
                bootstrap.download(url, destination, "0" * 64)
            except SystemExit as error:
                assert "Checksum mismatch" in str(error)
            else:
                raise AssertionError("Unverified response should not enter the cache")
            assert list(directory.iterdir()) == []
    finally:
        server.shutdown()
        worker.join(timeout=5)

with tempfile.TemporaryDirectory(prefix="gmux-extraction-test-") as directory:
    directory = Path(directory)
    archive_path = directory / "source.tar"
    with tarfile.open(archive_path, "w") as archive:
        for name, contents in [("source/build.zig", b"build"), ("source/data", b"x" * 4096)]:
            entry = tarfile.TarInfo(name)
            entry.size = len(contents)
            archive.addfile(entry, io.BytesIO(contents))
    complete = archive_path.read_bytes()
    archive_path.write_bytes(complete[:1600])
    destination = directory / "source"
    try:
        bootstrap.extract(archive_path, destination)
    except tarfile.ReadError:
        pass
    else:
        raise AssertionError("Truncated tar archive should fail extraction")
    assert not destination.exists(), "Incomplete source trees must not enter the cache"
    assert list(directory.iterdir()) == [archive_path], "Failed extraction must remove its temporary tree"
    destination.mkdir()
    (destination / "partial").write_bytes(b"older interrupted extraction")
    with tarfile.open(archive_path, "w") as archive:
        entry = tarfile.TarInfo("unexpected-root/build.zig")
        entry.size = 5
        archive.addfile(entry, io.BytesIO(b"build"))
    try:
        bootstrap.extract(archive_path, destination)
    except SystemExit as error:
        assert "does not contain source" in str(error)
    else:
        raise AssertionError("An unexpected archive root should fail validation")
    assert (destination / "partial").read_bytes() == b"older interrupted extraction"
    archive_path.write_bytes(complete)
    bootstrap.extract(archive_path, destination)
    assert not (destination / "partial").exists()
    assert (destination / "build.zig").read_bytes() == b"build"
    assert (destination / "data").read_bytes() == b"x" * 4096
    assert (destination / bootstrap.EXTRACTED).is_file()
    assert sorted(path.name for path in directory.iterdir()) == ["source", "source.tar"]

# Older versions extracted in place and could leave an existing partial tree.
with tempfile.TemporaryDirectory(prefix="gmux-legacy-bootstrap-test-") as directory:
    directory = Path(directory)
    source_archive = directory / "ghostty.tar.gz"
    source = directory / f"ghostty-{bootstrap.REVISION}"
    with tarfile.open(source_archive, "w") as archive:
        entry = tarfile.TarInfo(source.name + "/build.zig")
        entry.size = 5
        archive.addfile(entry, io.BytesIO(b"build"))
    checksum = hashlib.sha256(source_archive.read_bytes()).hexdigest()
    source.mkdir()
    (source / "README.md").write_text("older interrupted extraction")
    zig_dir = directory / f"zig-x86_64-linux-{bootstrap.ZIG_VERSION}"
    zig_dir.mkdir()
    (zig_dir / bootstrap.EXTRACTED).touch()
    (zig_dir / "zig").touch()

    def build(command, *, cwd, **kwargs):
        assert Path(cwd, "build.zig").read_bytes() == b"build", "Legacy partial source must be replaced before building"
        assert (source / bootstrap.EXTRACTED).is_file()
        return subprocess.CompletedProcess(command, 0, "")

    with patch.object(bootstrap, "DEPS", directory), patch.object(bootstrap, "SOURCE_SHA256", checksum), \
            patch.object(bootstrap.platform, "machine", return_value="x86_64"), \
            patch.object(bootstrap.platform, "system", return_value="Linux"), \
            patch.object(bootstrap.subprocess, "run", side_effect=build):
        bootstrap.main()
        (source / "local-cache").touch()
        bootstrap.main()
        assert (source / "local-cache").exists(), "Complete extracted sources should be reused"
print("PASS: interrupted download recovery, cache reuse, checksum failures, and atomic source extraction")
