//! Durable JSON-lines event streaming for headless runs.
//!
//! Events are published only after their transaction commits. A slow consumer
//! resynchronizes from the journal by sequence number, so the stream is
//! complete and duplicate-free even if the broadcast receiver lags.

use std::{
    fs::{self, OpenOptions},
    io::{self, BufWriter, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use lfm3_harness::{RunEvent, RunId, Store};
use tokio::sync::{broadcast, oneshot};

#[derive(Clone, Debug)]
pub enum EventTarget {
    Stdout,
    File(PathBuf),
}

impl EventTarget {
    /// Parses `-` as stdout and any other value as a file path.
    pub fn parse(raw: &str) -> Self {
        if raw == "-" {
            Self::Stdout
        } else {
            Self::File(PathBuf::from(raw))
        }
    }

    pub fn is_stdout(&self) -> bool {
        matches!(self, Self::Stdout)
    }

    pub fn description(&self) -> String {
        match self {
            Self::Stdout => "stdout".to_owned(),
            Self::File(path) => path.display().to_string(),
        }
    }

    fn open(&self) -> Result<Writer, String> {
        match self {
            Self::Stdout => Ok(Writer::new(Box::new(io::stdout()))),
            Self::File(path) => {
                if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
                    fs::create_dir_all(parent).map_err(|error| {
                        format!("create event directory {}: {error}", parent.display())
                    })?;
                }
                let file = OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open(path)
                    .map_err(|error| format!("open events {}: {error}", path.display()))?;
                Ok(Writer::new(Box::new(file)))
            }
        }
    }
}

struct Writer {
    inner: Mutex<BufWriter<Box<dyn Write + Send>>>,
}

impl Writer {
    fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            inner: Mutex::new(BufWriter::new(writer)),
        }
    }

    fn emit(&self, event: &RunEvent) -> Result<(), String> {
        let line = serde_json::to_string(event)
            .map_err(|error| format!("serialize event {}: {error}", event.seq))?;
        let mut writer = self
            .inner
            .lock()
            .map_err(|_| "event writer is poisoned".to_owned())?;
        writeln!(writer, "{line}")
            .and_then(|()| writer.flush())
            .map_err(|error| format!("write event: {error}"))
    }
}

/// A running streamer. Send on `stop` to finish; the task then flushes every
/// durable event it has not yet written and exits.
pub struct EventStream {
    stop: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<Result<(), String>>,
}

impl EventStream {
    pub fn spawn(store: Store, run_id: RunId, target: &EventTarget) -> Result<Self, String> {
        let writer = Arc::new(target.open()?);
        let (stop, stopped) = oneshot::channel();
        let handle = tokio::spawn(stream(store, run_id, writer, stopped));
        Ok(Self { stop, handle })
    }

    pub async fn finish(self) -> Result<(), String> {
        let _ = self.stop.send(());
        self.handle
            .await
            .map_err(|error| format!("event stream task: {error}"))?
    }
}

async fn stream(
    store: Store,
    run_id: RunId,
    writer: Arc<Writer>,
    mut stopped: oneshot::Receiver<()>,
) -> Result<(), String> {
    let mut receiver = store.subscribe();
    let mut last = 0_i64;
    last = resync(&store, &run_id, &writer, last).await?;

    loop {
        tokio::select! {
            biased;
            _ = &mut stopped => break,
            received = receiver.recv() => match received {
                Ok(event) => {
                    if event.run_id == run_id && event.seq > last {
                        writer.emit(&event)?;
                        last = event.seq;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    last = resync(&store, &run_id, &writer, last).await?;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    resync(&store, &run_id, &writer, last).await?;
    Ok(())
}

async fn resync(store: &Store, run_id: &RunId, writer: &Writer, after: i64) -> Result<i64, String> {
    let mut last = after;
    let events = store
        .events_after(run_id, after)
        .await
        .map_err(|error| format!("read durable events: {error}"))?;
    for event in events {
        writer.emit(&event)?;
        last = event.seq;
    }
    Ok(last)
}

/// Reads one whole event journal into a value array, used by `status`.
pub async fn read_events(store: &Store, run_id: &RunId) -> Result<Vec<RunEvent>, String> {
    store
        .events_after(run_id, 0)
        .await
        .map_err(|error| format!("read events: {error}"))
}

/// Opens an event target without starting a stream, for early validation.
pub fn check_target(target: &EventTarget) -> Result<(), String> {
    if let EventTarget::File(path) = target {
        if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
    }
    Ok(())
}
