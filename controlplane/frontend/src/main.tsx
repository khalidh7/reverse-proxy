import { useEffect, useRef, useState, type FormEvent } from 'react';
import { createRoot } from 'react-dom/client';
import './style.css';

type Service = {
  id: string;
  port: number;
  online: boolean;
  process: 'unmanaged' | 'running' | 'stopping';
};

type Metrics = {
  requests: number;
  upstream_responses: number;
  gateway_errors: number;
  server_errors: number;
};

type Report = {
  requests: number;
  concurrency: number;
  successful: number;
  failed: number;
  elapsed_ms: number;
  requests_per_second: number;
  mean_ms: number;
  p50_ms: number;
  p95_ms: number;
  p99_ms: number;
};

type Workload =
  | { state: 'idle' | 'running' | 'stopping' | 'stopped' }
  | { state: 'failed'; message: string }
  | { state: 'completed'; result: Report };

type Algorithm = 'round_robin' | 'least_active';

type Upstream = {
  id: string;
  origin: string;
  enabled: boolean;
  healthy: boolean;
  active_requests: number;
  requests: number;
};

type Routing = {
  version: number;
  algorithm: Algorithm;
  upstreams: Upstream[];
};

type Snapshot = {
  services: Service[];
  workload: Workload;
  proxy_metrics: Metrics | null;
  routing: Routing | null;
  service_controls: boolean;
};

const labels: Record<string, string> = {
  proxy: 'Reverse proxy',
  'app-1': 'App server 1',
  'app-2': 'App server 2',
  'app-3': 'App server 3',
};
const number = new Intl.NumberFormat(undefined, { maximumFractionDigits: 2 });

function App() {
  const [snapshot, setSnapshot] = useState<Snapshot>();
  const [connectionError, setConnectionError] = useState('');
  const [actionError, setActionError] = useState('');
  const [busy, setBusy] = useState(false);
  const [requests, setRequests] = useState('1000');
  const [concurrency, setConcurrency] = useState('16');
  const routingRevision = useRef(0);

  useEffect(() => {
    const controller = new AbortController();
    let timer: ReturnType<typeof setTimeout>;
    async function poll() {
      const revision = routingRevision.current;
      try {
        const response = await fetch('/api/status', { signal: controller.signal, cache: 'no-store' });
        if (!response.ok) throw new Error('Control plane status is unavailable.');
        const incoming: Snapshot = await response.json();
        if (revision === routingRevision.current) setSnapshot(incoming);
        setConnectionError('');
      } catch {
        if (!controller.signal.aborted) setConnectionError('Cannot reach the control plane.');
      } finally {
        if (!controller.signal.aborted) timer = setTimeout(poll, 750);
      }
    }
    void poll();
    return () => {
      controller.abort();
      clearTimeout(timer);
    };
  }, []);

  async function command(path: string, body?: object, method = 'POST') {
    setActionError('');
    setBusy(true);
    try {
      const response = await fetch(path, {
        method,
        headers: body ? { 'Content-Type': 'application/json' } : undefined,
        body: body ? JSON.stringify(body) : undefined,
      });
      if (!response.ok) throw new Error(await response.text());
      if (path === '/api/routing' || path === '/api/routing/clear-counts') {
        const routing: Routing = await response.json();
        routingRevision.current += 1;
        setSnapshot(current => current && { ...current, routing });
      }
    } catch (error) {
      setActionError(error instanceof Error ? error.message : 'Command failed.');
    } finally {
      setBusy(false);
    }
  }

  function start(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    void command('/api/workload/start', { requests: Number(requests), concurrency: Number(concurrency) });
  }

  const run = snapshot?.workload;
  const active = run?.state === 'running' || run?.state === 'stopping';
  const disabled = busy || Boolean(connectionError);
  const metrics = snapshot?.proxy_metrics;
  const routing = snapshot?.routing;

  function updateRouting(algorithm: Algorithm, upstreams: Upstream[]) {
    if (!routing) return;
    void command('/api/routing', {
      version: routing.version,
      algorithm,
      enabled_upstreams: upstreams.filter(upstream => upstream.enabled).map(upstream => upstream.id),
    }, 'PUT');
  }

  return (
    <main>
      <header className="page-heading">
        <div>
          <h1>Reverse Proxy Lab</h1>
          <p>Local HTTP workloads and service controls</p>
        </div>
        <code>127.0.0.1:8080</code>
      </header>
      {connectionError && <p className="error" role="alert">{connectionError}</p>}
      {actionError && <p className="error" role="alert">{actionError}</p>}
      <div className="columns">
        <section aria-labelledby="services-title">
          <h2 id="services-title">Services</h2>
          {!snapshot && <p>Waiting for service status…</p>}
          {snapshot?.services.map(service => (
            <div className="service" key={service.id}>
              <div>
                <strong>{labels[service.id]}</strong>
                <div className="subtle">:{service.port} · <span className={service.online ? 'online' : ''}>
                  {service.process === 'stopping' ? 'Stopping' : service.online ? 'Online' :
                    service.process === 'running' ? 'Starting' : 'Offline'}
                </span></div>
              </div>
              {snapshot.service_controls && (service.online && service.process === 'unmanaged' ? <small>External</small> :
                <button type="button" disabled={disabled || service.process === 'stopping'}
                  aria-label={`${service.process === 'running' ? 'Stop' : 'Start'} ${labels[service.id]}`}
                  onClick={() => void command(`/api/services/${service.id}/${service.process === 'running' ? 'stop' : 'start'}`)}>
                  {service.process === 'unmanaged' ? 'Start' : 'Stop'}
                </button>)}
            </div>
          ))}
        </section>
        <section aria-labelledby="workload-title">
          <h2 id="workload-title">Client workload</h2>
          <form onSubmit={start}>
            <label>Requests
              <input type="number" min="1" max="100000" required value={requests}
                onChange={event => setRequests(event.target.value)} disabled={active} />
            </label>
            <label>Concurrency
              <input type="number" min="1" max={Math.min(256, Number(requests))} required value={concurrency}
                onChange={event => setConcurrency(event.target.value)} disabled={active} />
            </label>
            <div className="actions">
              <button className="primary" disabled={disabled || active || !snapshot}>Run workload</button>
              <button type="button" disabled={disabled || run?.state !== 'running'}
                onClick={() => void command('/api/workload/stop')}>Stop</button>
            </div>
          </form>
          {run && <p className="run-state" role="status">{run.state}</p>}
          {run?.state === 'failed' && <p className="error">{run.message}</p>}
          {run?.state === 'completed' && <Stats items={[
            ['Successful', run.result.successful],
            ['Failed', run.result.failed],
            ['Requests/s', run.result.requests_per_second],
            ['Mean (ms)', run.result.mean_ms],
            ['p50 (ms)', run.result.p50_ms],
            ['p95 (ms)', run.result.p95_ms],
            ['p99 (ms)', run.result.p99_ms],
          ]} />}
        </section>
      </div>
      <section className="routing" aria-labelledby="routing-title">
        <div className="routing-heading">
          <h2 id="routing-title">Upstream routing</h2>
          <button type="button"
            disabled={disabled || active || !routing?.upstreams.some(upstream => upstream.requests > 0)}
            onClick={() => void command('/api/routing/clear-counts')}>
            Clear routed counts
          </button>
        </div>
        {routing ? <>
          <label className="algorithm">Algorithm
            <select value={routing.algorithm} disabled={disabled}
              onChange={event => updateRouting(event.target.value as Algorithm, routing.upstreams)}>
              <option value="round_robin">Round robin</option>
              <option value="least_active">Least active requests</option>
            </select>
          </label>
          <div className="table-scroll">
            <table>
              <thead><tr><th scope="col">Upstream</th><th scope="col">Health</th>
                <th scope="col">Active</th><th scope="col">Routed</th><th scope="col">Enabled</th></tr></thead>
              <tbody>{routing.upstreams.map(upstream => <tr key={upstream.id}>
                <th scope="row" title={upstream.origin}>{upstream.id}</th>
                <td className={upstream.healthy ? 'online' : 'subtle'}>
                  {upstream.healthy ? 'Healthy' : 'Unavailable'}
                </td>
                <td>{number.format(upstream.active_requests)}</td>
                <td>{number.format(upstream.requests)}</td>
                <td><input type="checkbox" aria-label={`Enable ${upstream.id}`}
                  checked={upstream.enabled} disabled={disabled}
                  onChange={event => {
                    const enabled = event.target.checked;
                    updateRouting(routing.algorithm, routing.upstreams.map(candidate =>
                      candidate.id === upstream.id ? { ...candidate, enabled } : candidate));
                  }} /></td>
              </tr>)}</tbody>
            </table>
          </div>
        </> : <p className="subtle">Waiting for proxy routing status.</p>}
      </section>
      <section aria-labelledby="metrics-title">
        <h2 id="metrics-title">Proxy metrics</h2>
        {metrics ? <Stats items={[
          ['Requests', metrics.requests],
          ['Upstream responses', metrics.upstream_responses],
          ['Gateway errors', metrics.gateway_errors],
          ['Total 5xx', metrics.server_errors],
        ]} /> : <p className="subtle">Metrics are unavailable while the proxy is offline.</p>}
      </section>
    </main>
  );
}

function Stats({ items }: { items: [string, number][] }) {
  return (
    <dl className="stats">
      {items.map(([label, value]) => <div key={label}>
        <dt>{label}</dt>
        <dd>{number.format(value)}</dd>
      </div>)}
    </dl>
  );
}

createRoot(document.getElementById('root')!).render(<App />);
