#!/usr/bin/env python3
"""Live CLI user-service and fd-activation checks; no model or installed config."""
import argparse
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time
import uuid


def command(*args):
    return subprocess.check_output(args, text=True).strip()


def query(path):
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(2)
        client.connect(str(path))
        request = {"schema": "agentlibre.agent.v1alpha",
                   "request_id": "req_01900000-0000-7000-8000-000000000001",
                   "command": {"type": "conversations", "workspace_path": None, "limit": 10}}
        client.sendall(json.dumps(request).encode() + b"\n")
        return json.loads(client.makefile("rb").readline())


def wait_ready(path, worker):
    until = time.monotonic() + 15
    while time.monotonic() < until:
        if worker.poll() is not None:
            raise AssertionError("daemon exited before readiness: " + worker.stdout.read())
        try:
            reply = query(path)
            assert reply["reply"]["status"] == "ok", reply
            return reply
        except (OSError, ValueError):
            time.sleep(.05)
    raise AssertionError("daemon socket did not become ready")


def run(agl, activated):
    with tempfile.TemporaryDirectory(prefix="agl-serve-", dir="/tmp") as name:
        root = Path(name)
        engine = root / "engine"
        engine.mkdir()
        executable = engine / "llama-server"
        executable.write_text("#!/bin/sh\nexit 99\n")
        executable.chmod(0o700)
        (engine / "libfixture.so").write_bytes(b"unused engine identity fixture")
        active_dir = root / "data/config"
        active_dir.mkdir(parents=True)
        (active_dir / "active.json").write_text(json.dumps({
            "schema": "agentlibre.active-config/v1", "source_digest": "fixture",
            "forge_sync_timeout_seconds": 300,
            "functions": [], "executable": str(executable), "search": None,
            "catalog_root": str(root / "data/source"),
            "service_git": {"memory_max_bytes": 1073741824, "max_output_bytes": 1048576},
            "keep_free_ram": None, "keep_free_vram": None,
        }))
        path = root / "state/daemon/agl.sock"
        path.parent.mkdir(parents=True, mode=0o700)
        env = dict(os.environ, AGL_HOME=str(root), WAYLAND_DISPLAY="agl-test-wayland")
        for key in ("LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES", "LISTEN_PIDFDID"):
            env.pop(key, None)
        unit = None
        if activated:
            unit = "agl-test-activation-" + uuid.uuid4().hex + ".service"
            args = ["systemd-run", "--user", "--quiet", "--wait", "--pipe", "--collect",
                    "--service-type=exec", "--expand-environment=no", "--unit=" + unit,
                    "--property=UMask=0077", "--property=TimeoutStopSec=2s",
                    "--setenv=AGL_HOME=" + str(root), "--setenv=PATH=" + env["PATH"],
                    "--", "systemd-socket-activate", "--fdname=agentlibre",
                    "--setenv=AGL_HOME", "--setenv=PATH", "--setenv=XDG_RUNTIME_DIR",
                    "--setenv=DBUS_SESSION_BUS_ADDRESS", "-l", str(path), str(agl), "serve"]
        else:
            args = [str(agl), "serve"]
        worker = subprocess.Popen(args, env=env, stdout=subprocess.PIPE,
                                  stderr=subprocess.STDOUT, text=True)
        if unit is None:
            unit = "agentlibre-daemon-shell-" + str(worker.pid) + ".service"
        try:
            wait_ready(path, worker)
            pid = int(command("systemctl", "--user", "show", unit, "-p", "MainPID", "--value"))
            assert pid > 0 and pid != worker.pid
            cgroup = Path("/proc", str(pid), "cgroup").read_text()
            assert unit in cgroup, cgroup
            assert Path("/proc", str(pid), "exe").resolve() == agl
            if not activated:
                environment = Path("/proc", str(pid), "environ").read_bytes().split(b"\0")
                assert b"WAYLAND_DISPLAY=agl-test-wayland" in environment
                worker.send_signal(signal.SIGTERM)
            else:
                subprocess.run(["systemctl", "--user", "stop", unit], check=True)
            worker.communicate(timeout=8)
            assert worker.returncode == 0, worker.returncode
            assert command("systemctl", "--user", "show", unit, "-p", "ActiveState", "--value") == "inactive"
            print("fd-activation passed" if activated else "shell user-service/environment/stop passed")
        finally:
            subprocess.run(["systemctl", "--user", "stop", unit], stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL)
            if worker.poll() is None:
                worker.terminate()
                worker.communicate(timeout=8)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--agl", type=Path, default=Path(__file__).resolve().parents[1] / "target/debug/agl")
    agl = parser.parse_args().agl.resolve()
    run(agl, False)
    run(agl, True)
