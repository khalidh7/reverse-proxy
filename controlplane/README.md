# Control plane

The Axum backend serves the compiled React dashboard and management API on `http://127.0.0.1:9090`. Build the workspace and frontend, then launch the backend from the repository root. The supervisor expects the `proxy`, `appserver`, and `client` executables next to its own binary. It uses the addresses from the default local topology.

| Endpoint | Method | Behavior |
| --- | --- | --- |
| `/api/status` | GET | Returns service health, process ownership, proxy metrics, and the latest workload state and result. |
| `/api/services/{id}/start` | POST | Launches one of `proxy`, `app-1`, `app-2`, or `app-3` with predefined arguments. |
| `/api/services/{id}/stop` | POST | Sends SIGTERM to the owned child and waits for exit. |
| `/api/workload/start` | POST | Launches the independent client with a validated JSON request such as `{"requests":1000,"concurrency":16}`. |
| `/api/workload/stop` | POST | Cancels the active client process. |

The supervisor launches binaries directly with argument arrays. It tracks one child per service and one active client run. Workload output comes from a bounded stdout pipe and results are kept in memory. A stopped workload has no partial report. Start and stop commands return `202 Accepted`. Poll the status endpoint to observe readiness or completion. A conflicting command returns `409 Conflict`.

The dashboard polls the backend. The backend probes proxy metrics on port 9080 and app management health on ports 9301–9303. Management binds to loopback. The control plane accepts its configured Host and same-origin browser requests. The standalone client performs all workload requests through the proxy data listener on port 8080.

Proxy and app server processes have independent lifetimes. They continue serving after control-plane exit, and a restarted control plane displays them as external services. The supervisor only stops processes it owns. The active client is scoped to the control plane and is killed when its supervisor is dropped.
