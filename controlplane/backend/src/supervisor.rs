use std::{path::PathBuf, process::Stdio, time::Duration};

use axum::http::StatusCode;
use client::{Report, Workload};
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use serde::Serialize;
use tokio::{
    io::{AsyncReadExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, oneshot},
    time::timeout,
};

#[derive(Clone, Copy, Serialize)]
pub struct Service {
    pub id: &'static str,
    pub port: u16,
    pub management_port: u16,
}

pub const SERVICES: [Service; 4] = [
    Service {
        id: "proxy",
        port: 8080,
        management_port: 9080,
    },
    Service {
        id: "app-1",
        port: 3001,
        management_port: 9301,
    },
    Service {
        id: "app-2",
        port: 3002,
        management_port: 9302,
    },
    Service {
        id: "app-3",
        port: 3003,
        management_port: 9303,
    },
];

pub type ApiError = (StatusCode, String);

struct Process {
    pid: u32,
    stop: Option<oneshot::Sender<()>>,
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Run {
    Idle,
    Running,
    Stopping,
    Completed { result: Report },
    Stopped,
    Failed { message: String },
}

pub struct Supervisor {
    binary_dir: PathBuf,
    root: PathBuf,
    container: bool,
    inner: Mutex<Inner>,
}

struct Inner {
    services: [Option<Process>; SERVICES.len()],
    client_stop: Option<oneshot::Sender<()>>,
    run: Run,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    Unmanaged,
    Running,
    Stopping,
}

impl Supervisor {
    pub fn new(binary_dir: PathBuf, root: PathBuf, container: bool) -> Self {
        Self {
            binary_dir,
            root,
            container,
            inner: Mutex::new(Inner {
                services: std::array::from_fn(|_| None),
                client_stop: None,
                run: Run::Idle,
            }),
        }
    }

    pub async fn start_service(self: &std::sync::Arc<Self>, id: &str) -> Result<(), ApiError> {
        if self.container {
            return Err(conflict("use Docker Compose to manage services"));
        }
        let (index, service) = service(id)?;
        let mut inner = self.inner.lock().await;
        if inner.services[index].is_some() {
            return Err(conflict("service already managed"));
        }
        let mut command = if index == 0 {
            let mut command = self.command("proxy");
            command.arg("proxy/config/development.toml");
            command
        } else {
            let mut command = self.command("appserver");
            command.args([
                service.id,
                &format!("127.0.0.1:{}", service.port),
                &format!("127.0.0.1:{}", service.management_port),
            ]);
            command
        };
        let mut child = command.spawn().map_err(spawn_error)?;
        let pid = child.id().expect("new child has a PID");
        let (stop, mut stop_requested) = oneshot::channel();
        inner.services[index] = Some(Process {
            pid,
            stop: Some(stop),
        });
        let supervisor = self.clone();
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status,
                Ok(()) = &mut stop_requested => stop_service(&mut child, pid).await,
            };
            tracing::info!(service = service.id, ?status, "service exited");
            supervisor.inner.lock().await.services[index] = None;
        });
        tracing::info!(service = service.id, pid, "service started");
        Ok(())
    }

    pub async fn stop_service(&self, id: &str) -> Result<(), ApiError> {
        let (index, _) = service(id)?;
        let mut inner = self.inner.lock().await;
        let Some(process) = &mut inner.services[index] else {
            return Err(conflict("service is not managed by this control plane"));
        };
        let stop = process
            .stop
            .take()
            .ok_or_else(|| conflict("service is already stopping"))?;
        let _ = stop.send(());
        tracing::info!(service = id, pid = process.pid, "service stop requested");
        Ok(())
    }

    pub async fn start_client(
        self: &std::sync::Arc<Self>,
        workload: Workload,
    ) -> Result<(), ApiError> {
        workload
            .validate()
            .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
        let mut inner = self.inner.lock().await;
        if matches!(inner.run, Run::Running | Run::Stopping) {
            return Err(conflict("a workload is already active"));
        }
        let mut child = self
            .command("client")
            .args([
                if self.container {
                    "http://proxy:8080/"
                } else {
                    "http://127.0.0.1:8080/"
                },
                &workload.requests.to_string(),
                &workload.concurrency.to_string(),
            ])
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(spawn_error)?;
        let stdout = child.stdout.take().expect("piped stdout");
        let (stop, stop_requested) = oneshot::channel();
        inner.client_stop = Some(stop);
        inner.run = Run::Running;
        let supervisor = self.clone();
        tokio::spawn(async move {
            let run = tokio::select! {
                biased;
                _ = stop_requested => match child.kill().await {
                    Ok(()) => Run::Stopped,
                    Err(error) => Run::Failed { message: error.to_string() },
                },
                result = async {
                    let mut output = Vec::new();
                    BufReader::new(stdout).take(4097).read_to_end(&mut output).await?;
                    anyhow::ensure!(output.len() <= 4096, "client report exceeds 4096 bytes");
                    let status = child.wait().await?;
                    anyhow::ensure!(status.success(), "client exited with {status}");
                    Ok::<_, anyhow::Error>(serde_json::from_slice(&output)?)
                } => match result {
                    Ok(result) => Run::Completed { result },
                    Err(error) => Run::Failed { message: error.to_string() },
                },
            };
            let mut inner = supervisor.inner.lock().await;
            inner.client_stop = None;
            inner.run = run;
        });
        Ok(())
    }

    pub async fn stop_client(&self) -> Result<(), ApiError> {
        let mut inner = self.inner.lock().await;
        let stop = inner
            .client_stop
            .take()
            .ok_or_else(|| conflict("no running workload to stop"))?;
        let _ = stop.send(());
        inner.run = Run::Stopping;
        Ok(())
    }

    pub async fn snapshot(&self) -> (serde_json::Value, [ProcessState; SERVICES.len()]) {
        let inner = self.inner.lock().await;
        let states = std::array::from_fn(|index| match &inner.services[index] {
            Some(process) if process.stop.is_some() => ProcessState::Running,
            Some(_) => ProcessState::Stopping,
            None => ProcessState::Unmanaged,
        });
        (serde_json::json!(&inner.run), states)
    }

    fn command(&self, binary: &str) -> Command {
        let mut command = Command::new(self.binary_dir.join(binary));
        command
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .process_group(0);
        command
    }
}

async fn stop_service(child: &mut Child, pid: u32) -> std::io::Result<std::process::ExitStatus> {
    // Check the child first so a process that just exited needs no signal.
    if let Some(status) = child.try_wait()? {
        return Ok(status);
    }
    if let Err(error) = kill(Pid::from_raw(pid as i32), Signal::SIGTERM) {
        tracing::warn!(pid, %error, "could not send SIGTERM");
    }
    if let Ok(result) = timeout(Duration::from_secs(12), child.wait()).await {
        return result;
    }
    child.kill().await?;
    child.wait().await
}

fn service(id: &str) -> Result<(usize, Service), ApiError> {
    SERVICES
        .iter()
        .copied()
        .enumerate()
        .find(|(_, service)| service.id == id)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "unknown service".into()))
}

fn spawn_error(error: std::io::Error) -> ApiError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("could not start binary: {error}"),
    )
}

fn conflict(message: &str) -> ApiError {
    (StatusCode::CONFLICT, message.into())
}
