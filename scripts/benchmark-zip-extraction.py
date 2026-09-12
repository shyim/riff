#!/usr/bin/env python3
"""Compare Riff ZIP installs with local HTTP fixtures and isolated package caches.

The manifest maps workload names to lists of ZIP paths. Both binaries install the
same generated lockfile. Warmups check extracted paths, bytes and modes; measured
runs rotate order and exclude fixture setup/cleanup. Only the standard library
and /usr/bin/time are required.
"""

import argparse
import hashlib
import http.server
import json
import math
import os
from pathlib import Path
import shutil
import statistics
import random
import subprocess
import tempfile
import threading
import time


def digest_tree(root):
    digest = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        digest.update(str(path.relative_to(root)).encode())
        digest.update(str(path.stat().st_mode & 0o7777).encode())
        if path.is_file():
            digest.update(hashlib.sha256(path.read_bytes()).digest())
    return digest.hexdigest()


def paired_ratio_interval(results):
    samples = {variant: sorted((r for r in results if r["variant"] == variant),
                               key=lambda r: r["round"])
               for variant in ["baseline", "candidate"]}
    ratios = [math.log(candidate["seconds"] / baseline["seconds"])
              for baseline, candidate in zip(samples["baseline"], samples["candidate"])]
    if len(ratios) < 2:
        return None
    rng = random.Random(21372)
    bootstrap = sorted(math.exp(statistics.mean(rng.choices(ratios, k=len(ratios))))
                       for _ in range(10000))
    return [bootstrap[250], bootstrap[9749]]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, default=Path("target"))
    parser.add_argument("--runs", type=int, default=8)
    parser.add_argument("--chunk-delay-ms", type=float, default=0)
    parser.add_argument("--request-delay-ms", type=float, default=0)
    parser.add_argument("--checksums", action="store_true")
    parser.add_argument("--modes", nargs="+", choices=["cold", "chunked", "warm"], default=["cold", "chunked", "warm"])
    args = parser.parse_args()
    if args.runs < 1 or min(args.chunk_delay_ms, args.request_delay_ms) < 0:
        parser.error("runs must be positive and delays cannot be negative")
    binaries = {"baseline": args.baseline.resolve(), "candidate": args.candidate.resolve()}
    fixtures = json.loads(args.manifest.read_text())
    args.work_dir.mkdir(parents=True, exist_ok=True)
    system = os.uname()
    data = {"runs": [], "summary": [], "fixtures": {}, "settings": {
        "runs_per_variant": args.runs, "chunk_delay_ms": args.chunk_delay_ms,
        "request_delay_ms": args.request_delay_ms,
        "checksums": args.checksums, "modes": args.modes,
        "platform": " ".join((system.sysname, system.release, system.machine)), "binaries": {key: str(value) for key, value in binaries.items()},
        "binary_sha256": {key: hashlib.sha256(value.read_bytes()).hexdigest() for key, value in binaries.items()},
    }}
    flags = ["--no-scripts", "--no-plugins", "--no-audit", "--no-blocking", "--no-autoloader", "--no-progress", "--no-interaction", "--ignore-platform-reqs", "--quiet"]

    for workload, filenames in fixtures.items():
        bodies = [Path(name).expanduser().read_bytes() for name in filenames]
        data["fixtures"][workload] = [{"path": str(Path(name).expanduser().resolve()), "bytes": len(body), "sha256": hashlib.sha256(body).hexdigest()} for name, body in zip(filenames, bodies)]

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"
            chunked = False
            requests = 0
            requests_lock = threading.Lock()

            def log_message(self, *unused):
                pass

            def do_GET(self):
                with self.requests_lock:
                    type(self).requests += 1
                if args.request_delay_ms:
                    time.sleep(args.request_delay_ms / 1000)
                try:
                    index = int(self.path.removeprefix("/").removesuffix(".zip"))
                    body = bodies[index]
                except (ValueError, IndexError):
                    self.send_error(404)
                    return
                self.send_response(200)
                self.send_header("Connection", "close")
                if self.chunked:
                    self.send_header("Transfer-Encoding", "chunked")
                else:
                    self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                for start in range(0, len(body), 128 * 1024):
                    chunk = body[start:start + 128 * 1024]
                    if self.chunked:
                        self.wfile.write(f"{len(chunk):x}\r\n".encode())
                    self.wfile.write(chunk)
                    if self.chunked:
                        self.wfile.write(b"\r\n")
                    if args.chunk_delay_ms:
                        time.sleep(args.chunk_delay_ms / 1000)
                if self.chunked:
                    self.wfile.write(b"0\r\n\r\n")

        class Server(http.server.ThreadingHTTPServer):
            request_queue_size = 128

        server = Server(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryDirectory(prefix="zip-install-", dir=args.work_dir.resolve()) as temporary:
                root = Path(temporary)
                seed = root / "seed"
                seed.mkdir()
                packages = [{"name": f"bench/package-{i}", "version": "1.0.0", "type": "library", "dist": {
                    "type": "zip", "url": f"http://127.0.0.1:{server.server_port}/{i}.zip",
                    "shasum": hashlib.sha1(body).hexdigest() if args.checksums else "",
                }} for i, body in enumerate(bodies)]
                composer = {"name": "bench/project", "require": {p["name"]: "1.0.0" for p in packages}, "repositories": [
                    {"type": "package", "package": packages}, {"packagist.org": False}], "config": {"secure-http": False}}
                (seed / "composer.json").write_text(json.dumps(composer))
                env = {**os.environ, "RIFF_CACHE_DIR": str(root / "seed-cache"), "COMPOSER_HOME": str(root / "composer-home")}
                subprocess.run([str(binaries["baseline"]), "update", "--no-install", *flags, "-d", str(seed)], env=env, check=True, capture_output=True)
                lock = (seed / "composer.lock").read_bytes()
                assert len(json.loads(lock)["packages"]) == len(packages)

                def run(variant, mode, warmup=False):
                    with tempfile.TemporaryDirectory(dir=root, prefix="project-") as project_dir:
                        project = Path(project_dir)
                        shutil.copy(seed / "composer.json", project / "composer.json")
                        (project / "composer.lock").write_bytes(lock)
                        cache = root / ("warm-cache" if mode == "warm" else "cold-cache")
                        if mode != "warm" and cache.exists():
                            shutil.rmtree(cache)
                        run_env = {**env, "RIFF_CACHE_DIR": str(cache)}
                        usage = project / "usage.txt"
                        requests_before = Handler.requests
                        started = time.perf_counter()
                        process = subprocess.run(["/usr/bin/time", "-f", "%U %S %M", "-o", str(usage), str(binaries[variant]), "install", *flags, "-d", str(project)], env=run_env, capture_output=True, text=True)
                        elapsed = time.perf_counter() - started
                        if process.returncode:
                            raise RuntimeError(f"{workload} {mode} {variant}: {process.stderr}")
                        user, system, rss = usage.read_text().split()
                        result = {"workload": workload, "mode": mode, "variant": variant, "seconds": elapsed,
                            "user_seconds": float(user), "sys_seconds": float(system), "peak_rss_kib": int(rss), "http_requests": Handler.requests - requests_before}
                        assert (project / "composer.lock").read_bytes() == lock
                        if warmup:
                            result["tree_sha256"] = digest_tree(project / "vendor" / "bench")
                            for i, body in enumerate(bodies):
                                archive = cache / "files" / "bench" / f"package-{i}" / f"bench-package-{i}-1.0.0.0.zip"
                                assert archive.read_bytes() == body, archive
                        else:
                            expected_requests = 0 if mode == "warm" else len(packages)
                            assert result["http_requests"] == expected_requests, result
                        return result

                expected_tree = None
                for mode in args.modes:
                    Handler.chunked = mode == "chunked"
                    for variant in binaries:
                        warmup = run(variant, mode, True)
                        if expected_tree is None:
                            expected_tree = warmup["tree_sha256"]
                        assert warmup["tree_sha256"] == expected_tree, (workload, mode, variant)
                    for repeat in range(args.runs):
                        order = list(binaries)
                        if repeat % 2:
                            order.reverse()
                        for variant in order:
                            result = run(variant, mode)
                            result["round"] = repeat
                            data["runs"].append(result)
                    results = [r for r in data["runs"] if r["workload"] == workload and r["mode"] == mode]
                    summary = {"workload": workload, "mode": mode, "tree_sha256": expected_tree}
                    for variant in binaries:
                        selected = [r for r in results if r["variant"] == variant]
                        summary[variant] = {"median_ms": statistics.median(r["seconds"] for r in selected) * 1000,
                            "median_peak_rss_kib": statistics.median(r["peak_rss_kib"] for r in selected)}
                    summary["time_reduction_pct"] = (1 - summary["candidate"]["median_ms"] / summary["baseline"]["median_ms"]) * 100
                    summary["paired_ratio_95pct_interval"] = paired_ratio_interval(results)
                    data["summary"].append(summary)
                    print(json.dumps(summary), flush=True)
                    args.output.parent.mkdir(parents=True, exist_ok=True)
                    args.output.write_text(json.dumps(data, indent=2) + "\n")
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    main()
