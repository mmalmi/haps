#!/usr/bin/env python3
"""One bounded package-index refresh. Run from a timer with a dedicated Haps home."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time


def size(paths):
    total = 0
    for path in paths:
        for directory, _, files in os.walk(path, followlinks=False):
            for name in files:
                try:
                    total += (Path(directory) / name).lstat().st_size
                except FileNotFoundError:
                    # A completed cache generation may retire while we measure it.
                    pass
    return total


def run(args):
    home = args.home.resolve()
    output = args.output.resolve()
    if home == output or home in output.parents or output in home.parents:
        raise RuntimeError("worker home and public output must be separate directories")
    home.mkdir(parents=True, exist_ok=True)
    output.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, HAPS_HOME=str(home), HAPS_NON_INTERACTIVE="true")
    if not (home / "identity.key").is_file():
        raise RuntimeError("initialize this worker's Haps identity before starting it")
    limit = args.max_state_mib * 1024 ** 2
    floor = args.min_free_gib * 1024 ** 3
    deadline = time.monotonic() + args.timeout

    def check():
        if min(shutil.disk_usage(home).free, shutil.disk_usage(output).free) < floor:
            raise RuntimeError("disk headroom floor reached; retaining previous publication")
        if size([home, output]) > limit:
            raise RuntimeError("index storage budget reached; retaining previous publication")
        if time.monotonic() >= deadline:
            raise RuntimeError("index worker deadline reached")

    def execute(command):
        check()
        process = subprocess.Popen(command, env=env, start_new_session=True)
        try:
            while process.poll() is None:
                check()
                time.sleep(1)
            if process.returncode:
                raise RuntimeError(f"{command[0]} exited with {process.returncode}")
            check()
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()

    with (home / "index-worker.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        execute([args.haps, "index", "build", "--out", str(output)])
        event = json.loads((output / "index.json").read_text())
        if json.loads(event["content"])["root"] is None:
            raise RuntimeError("no package announcements collected; refusing an empty publication")
        marker = home / "index-published.json"
        if marker.exists() and json.loads(marker.read_text())["id"] == event["id"]:
            print("Package index unchanged; no upload needed")
            return
        execute([args.htree, "add", str(output), "--publish", args.name])
        pending = marker.with_suffix(".pending")
        pending.write_text(json.dumps({"id": event["id"]}) + "\n")
        pending.replace(marker)
        print("Package index published")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--home", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--name", required=True)
    parser.add_argument("--haps", default="haps")
    parser.add_argument("--htree", default="htree")
    parser.add_argument("--timeout", type=int, default=90)
    parser.add_argument("--min-free-gib", type=float, default=2)
    parser.add_argument("--max-state-mib", type=int, default=128)
    run(parser.parse_args())
