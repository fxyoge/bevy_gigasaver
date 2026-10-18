#!/usr/bin/env python3
"""Run Divan suites serially and record output and environment."""

import argparse
import codecs
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import selectors
import signal
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parent.parent
BENCHES = ("snapshot", "changed", "explicit_dirty", "mixed", "lifecycle")


def run(command, *, env=None):
    return subprocess.run(
        command, cwd=ROOT, env=env, text=True, capture_output=True, check=True
    )


def interrupt(_signal, _frame):
    raise KeyboardInterrupt


def stream(command, env, timeout):
    """Keep child output visible and terminate its whole process group on abort."""
    chunks = []
    started = time.monotonic()
    heartbeat = started + 10
    interrupted = timed_out = False
    decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
    with subprocess.Popen(command, cwd=ROOT, env=env, stdout=subprocess.PIPE,
                          stderr=subprocess.STDOUT, start_new_session=True) as child:
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(child.stdout, selectors.EVENT_READ)
                while selector.get_map():
                    for key, _ in selector.select(timeout=1):
                        data = os.read(key.fileobj.fileno(), 65536)
                        if not data:
                            selector.unregister(key.fileobj)
                            continue
                        text = decoder.decode(data)
                        chunks.append(text)
                        print(text, end="", flush=True)
                    now = time.monotonic()
                    if now >= heartbeat:
                        print(f"\n[still running: {now - started:.0f}s elapsed]", flush=True)
                        heartbeat = now + 10
                    if now - started >= timeout:
                        timed_out = True
                        break
        except KeyboardInterrupt:
            interrupted = True
        finally:
            if timed_out or interrupted:
                print("\nStopping benchmark process group...", flush=True)
                if child.poll() is None:
                    os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
            remainder = child.communicate()[0]
            chunks.append(decoder.decode(remainder, final=True))
    return {
        "returncode": child.returncode,
        "timed_out": timed_out,
        "interrupted": interrupted,
        "output": "".join(chunks),
    }


def main():
    # Harness cancellation often sends SIGTERM rather than Ctrl-C. Still clean
    # up benchmark grandchildren, which run in their own process group.
    signal.signal(signal.SIGTERM, interrupt)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--temp-root", type=Path, default=ROOT / ".bench-tmp")
    parser.add_argument("--profile", choices=("quick", "full"), default="quick")
    parser.add_argument("--sample-count", type=int,
                        help="Override the profile's sample count")
    parser.add_argument("--timeout", type=float, default=180,
                        help="Hard timeout in seconds per suite (default: 180)")
    parser.add_argument("--filter", help="Run only case names matching this Divan pattern")
    parser.add_argument("--bench", choices=BENCHES, action="append",
                         help="Run selected suites; repeat to select more than one")
    parser.add_argument("--frames", action="store_true",
                        help="Measure 1200 lifecycle frames, separating idle/save percentiles")
    args = parser.parse_args()
    count = args.sample_count if args.sample_count is not None else (5 if args.profile == "quick" else 20)
    if count < 1 or args.timeout <= 0:
        parser.error("sample count and timeout must be positive")
    if args.frames and (args.filter or args.bench not in (None, ["lifecycle"])):
        parser.error("--frames only supports the lifecycle suite and no filter")
    suites = ["lifecycle"] if args.frames else (args.bench or list(BENCHES))

    # Build before sampling. Disk suites run serially to avoid self-interference.
    subprocess.run(["cargo", "bench", "--no-run", "--locked"], cwd=ROOT, check=True)
    args.temp_root.mkdir(parents=True, exist_ok=True)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    sources = [ROOT / "Cargo.toml", ROOT / "Cargo.lock", Path(__file__).resolve()]
    sources += sorted((ROOT / "src").rglob("*.rs"))
    sources += sorted((ROOT / "benches").rglob("*.rs"))
    report = {
        "recorded_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "platform": platform.platform(),
        "cpu": run(["lscpu"]).stdout,
        "cpu_affinity": sorted(os.sched_getaffinity(0)),
        "rustc": run(["rustc", "-Vv"]).stdout,
        "cargo": run(["cargo", "--version"]).stdout.strip(),
        "git_commit": run(["git", "rev-parse", "HEAD"]).stdout.strip(),
        "git_dirty": bool(run(["git", "status", "--porcelain"]).stdout),
        "source_sha256": {
            str(path.relative_to(ROOT)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in sources
        },
        "filesystem": run(["df", "-T", str(args.temp_root)]).stdout,
        "rustflags": os.environ.get("RUSTFLAGS", ""),
        "profile": args.profile,
        "status": "running",
        "requested_suites": suites,
        "sqlite_policy": "bundled SQLite; WAL; synchronous=FULL; automatic checkpointing",
        "method": (
            "1200 updates at 20ms real elapsed time per update; autosave every 1s; "
            "10 component mutations per update. Initial baseline excluded. "
            "Actual update wall-time percentiles, idle and save frames separated. "
            "Single local run; uncontrolled host activity and CPU frequency."
        ) if args.frames else (
            "Warm-cache, steady-baseline fixtures. Setup/baseline/destruction excluded. "
            "Commit includes simulation and synchronous persistence. Sustained samples "
            "contain 120 updates / 12 commits. Divan samples are not frame percentiles. "
            "Single local run; uncontrolled host activity and CPU frequency. "
            "Direct SQLite is a lower-level reference with a smaller schema."
        ),
        "benchmarks": [],
    }
    with tempfile.TemporaryDirectory(prefix="gigasaver-", dir=args.temp_root) as temporary:
        env = dict(os.environ, TMPDIR=str(Path(temporary).resolve()))
        for name in suites:
            command = ["cargo", "bench", "--locked", "--bench", name, "--"]
            command += ["--frames"] if args.frames else ["--color", "never", "--sample-count", str(count)]
            if args.profile == "quick" and not args.frames:
                command += ["--max-time", "2"]
                if not args.filter:
                    command += ["--skip", "sustained"]
            if args.filter:
                command.append(args.filter)
            print("Running:", " ".join(command), flush=True)
            started = time.monotonic()
            completed = stream(command, env, args.timeout)
            report["benchmarks"].append({
                "name": name,
                "command": command,
                "wall_seconds_including_fixtures": time.monotonic() - started,
                **completed,
            })
            # Preserve completed suites if a later command fails.
            args.output.write_text(json.dumps(report, indent=2) + "\n")
            if completed["returncode"] != 0:
                report["status"] = "interrupted" if completed["interrupted"] else "failed"
                args.output.write_text(json.dumps(report, indent=2) + "\n")
                print("Partial results recorded:", args.output, flush=True)
                return 130 if completed["interrupted"] else 1
    report["status"] = "complete"
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print("Recorded:", args.output)
    return 0


if __name__ == "__main__":
    sys.exit(main())
