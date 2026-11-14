import { useEffect, useState, type FormEvent } from 'react';
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

type Snapshot = {
  services: Service[];
  workload: Workload;
  proxy_metrics: Metrics | null;
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

  useEffect(() => {
    const controller = new AbortController();
    let timer: ReturnType<typeof setTimeout>;
    async function poll() {
      try {
        const response = await fetch('/api/status', { signal: controller.signal, cache: 'no-store' });
        if (!response.ok) throw new Error('Control plane status is unavailable.');
        setSnapshot(await response.json());
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

  async function command(path: string, body?: object) {
    setActionError('');
    setBusy(true);
    try {
      const response = await fetch(path, {
        method: 'POST',
        headers: body ? { 'Content-Type': 'application/json' } : undefined,
        body: body ? JSON.stringify(body) : undefined,
      });
      if (!response.ok) throw new Error(await response.text());
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
              {service.online && service.process === 'unmanaged' ? <small>External</small> :
                <button type="button" disabled={disabled || service.process === 'stopping'}
                  aria-label={`${service.process === 'running' ? 'Stop' : 'Start'} ${labels[service.id]}`}
                  onClick={() => void command(`/api/services/${service.id}/${service.process === 'running' ? 'stop' : 'start'}`)}>
                  {service.process === 'unmanaged' ? 'Start' : 'Stop'}
                </button>}
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
