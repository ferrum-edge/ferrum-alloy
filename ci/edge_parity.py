"""Compare Alloy's copies of two Ferrum Edge rules with a real Edge.

Alloy repeats two decisions Edge makes, and must reach the same verdicts:

* the canonical policy path (Edge `canonicalize_policy_path`, ported in
  crates/ferrum-alloy-edge/src/policy_path.rs), and
* the client Edge resolves from `X-Forwarded-For` with
  `FERRUM_TRUSTED_PROXIES` (repeated by `GatewayContext::client_address`,
  crates/ferrum-alloy-edge/src/policy.rs).

The expected verdicts live in contracts/fixtures/edge-parity/, and unit tests
of those two modules check that Alloy reaches them. This script checks that
the Edge image given on the command line reaches them too. It serves a
header-echo backend on 127.0.0.1:8080 and runs Edge in file mode on the host
network, in front of it through one catch-all proxy (`allow_path_parameters`
on, so the per-proxy `;` rule is not part of the comparison), with a global
`stdout_logging` plugin whose access log carries Edge's resolved
`client_ip`. Then:

* every fixture path is sent as the literal request target: a canonical case
  must reach the backend with exactly the canonical path, and a rejected one
  must get Edge's `400` with the error for that reason, without reaching the
  backend;
* every forwarded-for case is sent from 127.0.0.1 with its inbound
  `X-Forwarded-For`: the backend must receive exactly `forwarded`, and Edge's
  access log must name `client`.

Run: python3 -I ci/edge_parity.py --image <image> --release <label>
Exit status 1 lists every disagreement; 2 means Edge could not be run.
"""

from __future__ import annotations

import argparse
import http.client
import http.server
import json
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
FIXTURES = REPO / "contracts/fixtures/edge-parity"
CONTAINER = "edge-parity"
BACKEND_PORT = 8080
EDGE_PORT = 8000

EDGE_CONFIG = """\
version: "1"
consumers: []
plugin_configs:
  - id: "parity-access-log"
    namespace: "ferrum"
    plugin_name: "stdout_logging"
    config: {}
    scope: "global"
    enabled: true
proxies:
  - id: "parity"
    name: "parity"
    namespace: "ferrum"
    listen_path: "/"
    strip_listen_path: false
    allow_path_parameters: true
    backend_scheme: "http"
    backend_host: "127.0.0.1"
    backend_port: 8080
    backend_connect_timeout_ms: 2000
    backend_read_timeout_ms: 30000
    backend_write_timeout_ms: 30000
upstreams: []
"""


class Echo(http.server.BaseHTTPRequestHandler):
    """Answers with the request target and X-Forwarded-For it received."""

    seen: list[dict] = []

    def do_GET(self):
        record = {"path": self.path, "xff": self.headers.get_all("X-Forwarded-For") or []}
        Echo.seen.append(record)
        body = json.dumps(record).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def send(target: str, headers: dict[str, str] | None = None) -> tuple[int, str]:
    """Sends `target` byte for byte as the request target of one GET."""
    connection = http.client.HTTPConnection("127.0.0.1", EDGE_PORT, timeout=10)
    try:
        connection.putrequest("GET", target, skip_host=True, skip_accept_encoding=True)
        connection.putheader("Host", "localhost")
        for name, value in (headers or {}).items():
            connection.putheader(name, value)
        connection.endheaders()
        response = connection.getresponse()
        return response.status, response.read().decode("utf-8", "replace")
    finally:
        connection.close()


def docker(*args: str, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(["docker", *args], check=check, capture_output=True, text=True)


def start_edge(image: str, config_dir: Path, trusted: list[str]) -> None:
    docker("rm", "-f", CONTAINER, check=False)
    docker(
        "run", "-d", "--name", CONTAINER, "--network", "host",
        "-e", "FERRUM_MODE=file",
        "-e", "FERRUM_FILE_CONFIG_PATH=/config/edge.yaml",
        "-e", "FERRUM_NAMESPACE=ferrum",
        "-e", "FERRUM_ADMIN_HTTP_PORT=0",
        "-e", "FERRUM_TRUSTED_PROXIES=" + ",".join(trusted),
        "-v", f"{config_dir}:/config:ro",
        image, "run", "-m", "file",
    )
    for _ in range(60):
        try:
            status, _ = send("/ready")
            if status == 200:
                return
        except OSError:
            pass
        running = docker("inspect", "-f", "{{.State.Running}}", CONTAINER, check=False)
        if running.stdout.strip() != "true":
            raise RuntimeError("Edge exited before serving the parity configuration")
        time.sleep(1)
    raise RuntimeError("Edge did not serve the parity configuration within 60 s")


def access_log() -> list[dict]:
    """Edge's stdout_logging records: JSON lines with a client_ip."""
    logs = docker("logs", CONTAINER, check=False)
    records = []
    for line in (logs.stdout + logs.stderr).splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            record = json.loads(line)
        except ValueError:
            continue
        if isinstance(record, dict) and "client_ip" in record and "request_path" in record:
            records.append(record)
    return records


def check_paths(fixture: dict, failures: list[str]) -> int:
    errors = fixture["edge_rejection_errors"]
    for case in fixture["cases"]:
        path = case["path"]
        before = len(Echo.seen)
        status, body = send(path)
        reached = Echo.seen[before:]
        if "canonical" in case:
            expected = case["canonical"]
            got = reached[-1]["path"] if reached else None
            if status != 200 or got != expected:
                failures.append(
                    f"path {path!r}: Alloy canonicalizes it to {expected!r}; "
                    f"Edge answered {status} and the backend received {got!r}"
                )
        else:
            reason = case["rejection"]
            try:
                error = json.loads(body).get("error")
            except (ValueError, AttributeError):
                error = None
            if status != 400 or error != errors[reason] or reached:
                failures.append(
                    f"path {path!r}: Alloy refuses it ({reason}); Edge answered {status} "
                    f"{body.strip()[:200]!r}" + (" and forwarded it" if reached else "")
                )
    return len(fixture["cases"])


def check_forwarded(fixture: dict, failures: list[str]) -> int:
    cases = fixture["cases"]
    for index, case in enumerate(cases):
        before = len(Echo.seen)
        status, _ = send(f"/xff/{index}", {"X-Forwarded-For": case["x_forwarded_for"]})
        reached = Echo.seen[before:]
        got = ", ".join(reached[-1]["xff"]) if reached else None
        if status != 200 or got != case["forwarded"]:
            failures.append(
                f"X-Forwarded-For {case['x_forwarded_for']!r}: expected Edge to forward "
                f"{case['forwarded']!r}; it answered {status} and forwarded {got!r}"
            )
    # The access log is written off the request path; wait for every record.
    wanted = {f"/xff/{index}" for index in range(len(cases))}
    resolved: dict[str, str] = {}
    for _ in range(40):
        resolved = {
            record["request_path"]: record["client_ip"]
            for record in access_log()
            if record["request_path"] in wanted
        }
        if len(resolved) == len(wanted):
            break
        time.sleep(0.5)
    for index, case in enumerate(cases):
        got = resolved.get(f"/xff/{index}")
        if got != case["client"]:
            failures.append(
                f"X-Forwarded-For {case['x_forwarded_for']!r}: Alloy resolves client "
                f"{case['client']}; Edge's access log says {got!r}"
            )
    return len(cases)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--image", required=True, help="Ferrum Edge image to run")
    parser.add_argument("--release", required=True, help="label for messages")
    args = parser.parse_args()

    paths = json.loads((FIXTURES / "policy-paths.json").read_text(encoding="utf-8"))
    forwarded = json.loads((FIXTURES / "forwarded-for.json").read_text(encoding="utf-8"))

    backend = http.server.ThreadingHTTPServer(("127.0.0.1", BACKEND_PORT), Echo)
    threading.Thread(target=backend.serve_forever, daemon=True).start()
    failures: list[str] = []
    try:
        with tempfile.TemporaryDirectory() as config_dir:
            Path(config_dir, "edge.yaml").write_text(EDGE_CONFIG, encoding="utf-8")
            Path(config_dir).chmod(0o755)
            Path(config_dir, "edge.yaml").chmod(0o644)
            try:
                start_edge(args.image, Path(config_dir), forwarded["trusted_proxies"])
            except (RuntimeError, subprocess.CalledProcessError) as error:
                print(f"::error::{error}")
                print(docker("logs", CONTAINER, check=False).stdout[-4000:])
                return 2
            path_count = check_paths(paths, failures)
            forwarded_count = check_forwarded(forwarded, failures)
    finally:
        if failures:
            logs = docker("logs", CONTAINER, check=False)
            print((logs.stdout + logs.stderr)[-4000:])
        docker("rm", "-f", CONTAINER, check=False)
        backend.shutdown()

    for failure in failures:
        print(f"::error::Edge {args.release}: {failure}")
    if failures:
        return 1
    print(
        f"Edge {args.release} agrees with Alloy on {path_count} request paths "
        f"and {forwarded_count} forwarded-for chains."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
