# Reverse Proxy Lab

Reverse Proxy Lab is a local HTTP/1.1 lab with four independently executable components. The standalone client sends requests to the proxy, which routes them across healthy Axum app servers. The control plane runs a local process supervisor and serves a React dashboard for starting workloads and reading results.

## Run with Docker Compose

From the repository root, with Docker running:

```sh
docker compose up --build -d
```

Open [Reverse Proxy Lab](http://127.0.0.1:9090/). In **Upstream routing**, choose **Round robin** or **Least active requests**. Changes apply to new requests immediately. Use the **Enabled** checkboxes to take upstreams out of rotation and watch their **Routed** counts. Use **Run workload** to send traffic from the dashboard.

Compose publishes only the proxy and dashboard ports on host loopback. The upstreams and their management endpoints use the Compose network. The dashboard runs the standalone client binary inside the control-plane container. To run a separate one-off client container, use:

```sh
docker compose run --rm --build client http://proxy:8080/ 1000 16
```

Stop any active dashboard workload with **Stop**. Use `docker compose stop app-1` to test failover and `docker compose start app-1` to restore it. Use `docker compose down` to shut down the stack.

## Run directly on the host

From the repository root, use stable Rust and Node.js 22 or newer.

If `cargo --version` returns `command not found`, install Rust with
[rustup](https://rustup.rs/). Open a new terminal after installation, or run
`source ~/.cargo/env` to load Cargo in your current shell.

```sh
cargo build --workspace --locked &&
npm --prefix controlplane/frontend ci &&
npm --prefix controlplane/frontend run build &&
./target/debug/controlplane
```

Open [Reverse Proxy Lab](http://127.0.0.1:9090/). Start the three app servers and the reverse proxy from the **Services** panel. Wait for them to show **Online**. Run a workload with **1,000 requests** and **16 concurrent workers**. The dashboard displays successes, failures, throughput, mean latency, and p50, p95, and p99. It also shows cumulative proxy counters.

The default configuration uses all three app servers and round robin. Stop the proxy and app servers from the dashboard before closing the control plane when you want to shut down the whole lab.

## Verify the request path

```sh
curl -fsS http://127.0.0.1:8080/
curl -fsS -d 'hello proxy' http://127.0.0.1:8080/echo
curl -fsS 'http://127.0.0.1:8080/payload?bytes=4096' | wc -c
curl -fsS http://127.0.0.1:9080/metrics
```

Expect a greeting from a healthy enabled app server, the echoed payload, a byte count of 4096, and a JSON metrics snapshot. Stop app server 1 and wait for its routing health to show **Unavailable**. Requests should continue through the remaining servers. Disable all upstreams to see `503 Service Unavailable`, then enable a healthy upstream and request `/delay?ms=5500` to see a `504 Gateway Timeout` with the default five-second timeout.

## Run components independently

Run the three app server commands and the proxy command in separate terminals. The standalone client prints one JSON report and exits.

```sh
./target/debug/appserver app-1 127.0.0.1:3001 127.0.0.1:9301
./target/debug/appserver app-2 127.0.0.1:3002 127.0.0.1:9302
./target/debug/appserver app-3 127.0.0.1:3003 127.0.0.1:9303
./target/debug/proxy proxy/config/development.toml
./target/debug/client http://127.0.0.1:8080/ 1000 16
```

Each app server provides `GET /`, `GET /health`, `GET /payload?bytes=N`, `GET /delay?ms=N`, `POST /echo`, and `GET /fail` on its data listener. Payloads and echo requests are limited to 1 MiB. Simulated delays are limited to 10 seconds. A separate loopback listener exposes management health.

## Proxy design and limitations

- Hyper parses and serializes HTTP/1.1. The proxy builds the upstream URI from the configured origin and incoming path and query, regenerates `Host`, and removes hop-by-hop headers and common client-supplied forwarding headers. Hyper polls body data frames as the next hop can accept them. The proxy drops trailers and rejects CONNECT and protocol upgrades. [Hyper HTTP/1 server documentation](https://docs.rs/hyper/latest/hyper/server/conn/http1/)
- Tokio drives network I/O. The data listener caps concurrent downstream connections and leaves excess connections in the OS accept backlog. Hyper-util supplies upstream connection reuse. The proxy disables automatic retries and owns the connection cap, timeout, and shutdown policy. [Hyper-util client configuration](https://docs.rs/hyper-util/latest/hyper_util/client/legacy/struct.Builder.html)
- The upstream timeout ends when response headers arrive. A body error after headers have been sent ends the response stream. Counters record requests received, upstream responses whose headers arrived, generated gateway errors (including 503 when no upstream is eligible), and total 5xx responses. SIGINT and SIGTERM close the data listener and let active connections drain for up to the configured shutdown deadline.
- The proxy reads upstream origins, the initial algorithm, and timeouts from TOML. Existing single-upstream files using `upstream` also work. The proxy checks each upstream's data-plane `/health` endpoint every second by default and routes only to healthy, enabled upstreams. Connection failures observed before the next health check can return `502`.
- Round robin rotates through eligible upstreams. Least active requests counts work through the end or cancellation of the response stream and rotates among ties. Runtime algorithm and enable changes are applied together against a versioned snapshot. Settings reset to the TOML defaults when the proxy restarts.
- The client sends GET requests to one URL per run, with up to 100,000 requests and 256 workers. It includes response-body reads in latency, times out each attempt after 10 seconds, and counts HTTP non-2xx, transport errors, and timeouts as failures. Latency statistics include successful and failed attempts.

For the local management API and process behavior, see the [control plane README](controlplane/README.md).
