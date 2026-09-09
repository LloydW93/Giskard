use super::{CodexTransport, HarnessError};
use crate::rpc::{CodexStreamError, NON_JSON_STDOUT_PREVIEW_BYTES, bounded_utf8_preview};
use async_trait::async_trait;
use codex_codes::jsonrpc::{
    JsonRpcError, JsonRpcErrorData, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
    JsonRpcResponse, RequestId,
};
use codex_codes::{Notification, ServerMessage, ServerRequest};
use giskard_harness::{EventLog, EventLogReader, EventStreamError};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicI64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter,
};
use tokio::process::Child;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, error, trace, warn};

const STDOUT_BUFFER_SIZE: usize = 10 * 1024 * 1024;
const WRITER_QUEUE_CAPACITY: usize = 64;
pub(super) const CODEX_INBOX_RETAIN_LIMIT: usize = 65_536;
pub(super) const CODEX_MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

type Waiter = oneshot::Sender<Result<Value, HarnessError>>;
type Waiters = Arc<Mutex<HashMap<RequestId, Waiter>>>;

#[derive(Clone)]
enum InboxItem {
    Message(Box<ServerMessage>),
    NonJson {
        parse_error: String,
        raw_preview: String,
        raw_bytes: usize,
    },
    Fatal(String),
    Eof,
}

struct Frame {
    line: String,
    description: String,
    written: Option<oneshot::Sender<Result<(), HarnessError>>>,
    state: Option<Arc<AtomicU8>>,
}

struct Registration {
    waiters: Waiters,
    id: RequestId,
    method: String,
    state: Arc<AtomicU8>,
    abandoned_states: Option<Arc<Mutex<Vec<u8>>>>,
    armed: bool,
}

impl Registration {
    fn disarm(&mut self) {
        self.armed = false;
    }

    fn abandon(&mut self) {
        lock_waiters(&self.waiters).remove(&self.id);
        self.disarm();
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        lock_waiters(&self.waiters).remove(&self.id);
        let state = self.state.load(Ordering::Acquire);
        if let Some(states) = &self.abandoned_states {
            lock_mutex(states).push(state);
        }
        match state {
            0 => debug!(method = %self.method, request_id = %self.id,
                "Codex request timed out before it was queued"),
            1 => debug!(method = %self.method, request_id = %self.id,
                "Codex request timed out after it was queued but before it was written"),
            _ => debug!(method = %self.method, request_id = %self.id,
                "Codex request timed out after it was written"),
        }
    }
}

type NativeServiceFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), HarnessError>> + Send>>;

pub(super) struct StdioTransport {
    native_services: crate::NativeServiceProviders,
    deferred_inbox: VecDeque<InboxItem>,
    // Polled only by the transport owner. Keeping the future (including the child process and
    // write acknowledgment) here makes next_message cancellation resumable without reexecution.
    pending_service: Option<NativeServiceFuture>,
    service_error: Option<HarnessError>,
    deferred_overflow: bool,
    writer_tx: Option<mpsc::Sender<Frame>>,
    inbox: Arc<EventLog<InboxItem>>,
    inbox_reader: EventLogReader<InboxItem>,
    waiters: Waiters,
    next_id: Arc<AtomicI64>,
    child: Option<Child>,
    reader_task: Option<JoinHandle<()>>,
    writer_task: Option<JoinHandle<()>>,
    stderr_task: Option<JoinHandle<()>>,
}

impl StdioTransport {
    pub(super) async fn spawn(
        builder: codex_codes::AppServerBuilder,
    ) -> Result<Self, HarnessError> {
        let mut child = builder
            .spawn()
            .await
            .map_err(|error| HarnessError::Spawn(error.to_string()))?;
        let stdin = child.stdin.take().ok_or_else(|| {
            HarnessError::Spawn("Codex app-server stdin was not piped".to_owned())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            HarnessError::Spawn("Codex app-server stdout was not piped".to_owned())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            HarnessError::Spawn("Codex app-server stderr was not piped".to_owned())
        })?;
        let stderr_task = drain_stderr(stderr);
        Ok(Self::from_io(
            stdout,
            stdin,
            Some(child),
            Some(stderr_task),
            CODEX_INBOX_RETAIN_LIMIT,
        ))
    }

    fn from_io<R, W>(
        reader: R,
        writer: W,
        child: Option<Child>,
        stderr_task: Option<JoinHandle<()>>,
        inbox_limit: usize,
    ) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let inbox = Arc::new(EventLog::with_limit(inbox_limit));
        // The reader must exist before the producer starts. Otherwise eviction before the first
        // call to next_message would be invisible rather than reported as a Gap.
        let inbox_reader = inbox.reader();
        let waiters = Arc::new(Mutex::new(HashMap::new()));
        let (writer_tx, writer_rx) = mpsc::channel(WRITER_QUEUE_CAPACITY);
        let reader_task = tokio::spawn(read_stdout(reader, inbox.clone(), waiters.clone()));
        let writer_task = tokio::spawn(write_stdin(writer, writer_rx, waiters.clone()));
        Self {
            native_services: crate::NativeServiceProviders::default(),
            deferred_inbox: VecDeque::new(),
            pending_service: None,
            service_error: None,
            deferred_overflow: false,
            writer_tx: Some(writer_tx),
            inbox,
            inbox_reader,
            waiters,
            next_id: Arc::new(AtomicI64::new(1)),
            child,
            reader_task: Some(reader_task),
            writer_task: Some(writer_task),
            stderr_task,
        }
    }

    #[cfg(test)]
    fn from_pipes<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::from_io(reader, writer, None, None, CODEX_INBOX_RETAIN_LIMIT)
    }

    pub(super) fn set_native_services(&mut self, providers: crate::NativeServiceProviders) {
        self.native_services = providers;
    }

    // Services may be requested before the response to a client RPC. The same task and inbox
    // reader service them while waiting, retaining other frames in order for the instance.
    async fn service_inbox_item(
        &mut self,
        item: InboxItem,
    ) -> Result<Option<InboxItem>, HarnessError> {
        if let InboxItem::Message(message) = &item
            && let ServerMessage::Request { id, request } = message.as_ref()
            && crate::NativeServiceProviders::handles(request)
        {
            self.pending_service = Some(Box::pin(run_native_service(
                self.native_services.clone(),
                self.writer_tx.as_ref().cloned(),
                id.clone(),
                request.clone(),
            )));
            self.finish_pending_service().await?;
            return Ok(None);
        }
        Ok(Some(item))
    }

    async fn finish_pending_service(&mut self) -> Result<(), HarnessError> {
        if let Some(error) = &self.service_error {
            return Err(error.clone());
        }
        let Some(operation) = self.pending_service.as_mut() else {
            return Ok(());
        };
        let result = operation.await;
        // A ready future must never be polled again. A failed/uncertain write poisons this
        // connection instead of invoking the credential provider or sending the reply again.
        self.pending_service = None;
        if let Err(error) = &result {
            self.service_error = Some(error.clone());
            self.fail_waiters("native service response delivery failed");
        }
        result
    }

    pub(super) async fn send_notification(&mut self, method: &str) -> Result<(), HarnessError> {
        self.send_frame(
            &JsonRpcNotification {
                method: method.to_owned(),
                params: None,
            },
            format!("notification {method}"),
        )
        .await
    }

    async fn send_frame<T: Serialize>(
        &mut self,
        value: &T,
        description: String,
    ) -> Result<(), HarnessError> {
        send_frame(self.writer_tx.as_ref().cloned(), value, description).await
    }

    fn fail_waiters(&self, message: &str) {
        fail_all_waiters(&self.waiters, message);
    }
}

#[async_trait]
impl CodexTransport for StdioTransport {
    async fn request_json(&mut self, method: &str, params: Value) -> Result<Value, HarnessError> {
        self.finish_pending_service().await?;
        let response = request_json(
            self.writer_tx.as_ref().cloned(),
            self.waiters.clone(),
            self.next_id.clone(),
            method,
            params,
            None,
        );
        tokio::pin!(response);
        loop {
            tokio::select! {
                biased;
                result = &mut response => return result,
                item = self.inbox_reader.recv() => {
                    let item = item.map_err(|error| HarnessError::Transport(format!("Codex service inbox failed: {error:?}")))?;
                    if let Some(item) = self.service_inbox_item(item).await? {
                        if self.deferred_inbox.len() >= CODEX_INBOX_RETAIN_LIMIT {
                            self.deferred_overflow = true;
                            return Err(HarnessError::Transport("Codex deferred inbox overflowed".into()));
                        }
                        self.deferred_inbox.push_back(item);
                    }
                }
            }
        }
    }

    async fn next_message(&mut self) -> Result<Option<ServerMessage>, CodexStreamError> {
        if self.deferred_overflow {
            return Err(CodexStreamError::Fatal(HarnessError::Transport(
                "Codex deferred inbox overflowed".into(),
            )));
        }
        loop {
            self.finish_pending_service()
                .await
                .map_err(CodexStreamError::Fatal)?;
            let item = match self.deferred_inbox.pop_front() {
                Some(item) => Ok(item),
                None => self.inbox_reader.recv().await,
            };
            let item = match item {
                Ok(item) => match self
                    .service_inbox_item(item)
                    .await
                    .map_err(CodexStreamError::Fatal)?
                {
                    Some(item) => Ok(item),
                    None => continue,
                },
                Err(error) => Err(error),
            };
            return match item {
                Ok(InboxItem::Message(message)) => Ok(Some(*message)),
                Ok(InboxItem::NonJson {
                    parse_error,
                    raw_preview,
                    raw_bytes,
                }) => Err(CodexStreamError::NonJsonStdout {
                    parse_error,
                    raw_preview,
                    raw_bytes,
                }),
                Ok(InboxItem::Fatal(message)) => {
                    Err(CodexStreamError::Fatal(HarnessError::Transport(message)))
                }
                Ok(InboxItem::Eof) | Err(EventStreamError::Closed) => Ok(None),
                Err(EventStreamError::Gap { dropped }) => {
                    Err(CodexStreamError::Fatal(HarnessError::Transport(format!(
                        "Codex inbox overflowed; {dropped} frames dropped"
                    ))))
                }
            };
        }
    }

    async fn respond_json(&mut self, id: RequestId, value: Value) -> Result<(), HarnessError> {
        self.send_frame(
            &JsonRpcResponse {
                id: id.clone(),
                result: value,
            },
            format!("response id {id}"),
        )
        .await
    }

    async fn respond_error_json(
        &mut self,
        id: RequestId,
        code: i64,
        message: &str,
    ) -> Result<(), HarnessError> {
        self.send_frame(
            &JsonRpcError {
                id: id.clone(),
                error: JsonRpcErrorData {
                    code,
                    message: message.to_owned(),
                    data: None,
                },
            },
            format!("error response id {id}"),
        )
        .await
    }

    async fn shutdown_transport(mut self) -> Result<(), HarnessError> {
        // Drop provider I/O before closing the writer: the future owns a writer sender clone.
        self.pending_service.take();
        self.writer_tx.take();
        self.fail_waiters("Codex transport shut down");
        self.inbox.close();
        if let Some(writer_task) = self.writer_task.take() {
            let _ = writer_task.await;
        }
        if let Some(mut child) = self.child.take() {
            child
                .kill()
                .await
                .map_err(|error| HarnessError::Transport(error.to_string()))?;
        }
        if let Some(reader_task) = self.reader_task.take() {
            reader_task.abort();
            let _ = reader_task.await;
        }
        if let Some(stderr_task) = self.stderr_task.take() {
            stderr_task.abort();
        }
        Ok(())
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.pending_service.take();
        self.writer_tx.take();
        self.fail_waiters("Codex transport shut down");
        self.inbox.close();
        if let Some(child) = &mut self.child
            && let Err(error) = child.start_kill()
        {
            warn!(%error, "failed to kill Codex app-server while dropping transport");
        }
        if let Some(task) = &self.reader_task {
            task.abort();
        }
        if let Some(task) = &self.writer_task {
            task.abort();
        }
        if let Some(task) = &self.stderr_task {
            task.abort();
        }
    }
}

async fn run_native_service(
    providers: crate::NativeServiceProviders,
    writer_tx: Option<mpsc::Sender<Frame>>,
    id: RequestId,
    request: ServerRequest,
) -> Result<(), HarnessError> {
    let method = request.method();
    let response = match providers.response(&request).await {
        Ok(result) => serde_json::json!({"id": id, "result": result}),
        Err(message) => {
            warn!(action = "native_service_response", method, request_id = %id,
                error = message, "host provider could not answer Codex service request");
            serde_json::json!({"id": id, "error": {"code": -32000, "message": message}})
        }
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        send_frame(
            writer_tx,
            &response,
            format!("native service response id {id}"),
        ),
    )
    .await
    .map_err(|_| HarnessError::Timeout("native service response write timed out".into()))??;
    debug!(action = "native_service_response", method, request_id = %id,
        "answered Codex host service request");
    Ok(())
}

async fn request_json(
    writer_tx: Option<mpsc::Sender<Frame>>,
    waiters: Waiters,
    next_id: Arc<AtomicI64>,
    method: &str,
    params: Value,
    abandoned_states: Option<Arc<Mutex<Vec<u8>>>>,
) -> Result<Value, HarnessError> {
    let id = RequestId::Integer(next_id.fetch_add(1, Ordering::Relaxed));
    let (response_tx, response_rx) = oneshot::channel();
    let state = Arc::new(AtomicU8::new(0));
    lock_waiters(&waiters).insert(id.clone(), response_tx);
    let mut registration = Registration {
        waiters,
        id: id.clone(),
        method: method.to_owned(),
        state: state.clone(),
        abandoned_states,
        armed: true,
    };
    let line = match serialize_line(&JsonRpcRequest {
        id: id.clone(),
        method: method.to_owned(),
        params: Some(params),
    }) {
        Ok(line) => line,
        Err(error) => {
            registration.abandon();
            return Err(error);
        }
    };
    let Some(writer_tx) = writer_tx else {
        registration.abandon();
        return Err(transport_closed());
    };
    if writer_tx
        .send(Frame {
            line,
            description: format!("request {method} id {id}"),
            written: None,
            state: Some(state.clone()),
        })
        .await
        .is_err()
    {
        registration.abandon();
        return Err(transport_closed());
    }
    // The writer can finish before send returns. Do not overwrite its stronger state.
    let _ = state.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire);
    match response_rx.await {
        Ok(result) => {
            registration.disarm();
            result
        }
        Err(_) => {
            registration.abandon();
            Err(transport_closed())
        }
    }
}

async fn send_frame<T: Serialize>(
    writer_tx: Option<mpsc::Sender<Frame>>,
    value: &T,
    description: String,
) -> Result<(), HarnessError> {
    let line = serialize_line(value)?;
    let (written_tx, written_rx) = oneshot::channel();
    writer_tx
        .ok_or_else(transport_closed)?
        .send(Frame {
            line,
            description,
            written: Some(written_tx),
            state: None,
        })
        .await
        .map_err(|_| transport_closed())?;
    written_rx.await.map_err(|_| transport_closed())?
}

async fn read_stdout<R>(reader: R, inbox: Arc<EventLog<InboxItem>>, waiters: Waiters)
where
    R: AsyncRead + Unpin,
{
    let mut reader = BufReader::with_capacity(STDOUT_BUFFER_SIZE, reader);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        let mut limited = (&mut reader).take(CODEX_MAX_FRAME_BYTES as u64 + 1);
        match limited.read_until(b'\n', &mut buffer).await {
            Ok(0) => {
                inbox.append(InboxItem::Eof);
                fail_all_waiters(&waiters, "Codex stream closed");
                inbox.close();
                return;
            }
            Ok(_) => {}
            Err(error) => {
                inbox.append(InboxItem::Fatal(format!(
                    "failed to read Codex stdout: {error}"
                )));
                fail_all_waiters(&waiters, "Codex stream read failed");
                inbox.close();
                return;
            }
        }
        if buffer.len() > CODEX_MAX_FRAME_BYTES {
            inbox.append(InboxItem::Fatal(format!(
                "Codex stdout frame exceeded {CODEX_MAX_FRAME_BYTES} bytes"
            )));
            fail_all_waiters(&waiters, "Codex stream produced an oversized frame");
            inbox.close();
            return;
        }
        let mut line = String::from_utf8_lossy(&buffer).into_owned();
        trim_line_ending(&mut line);
        if line.trim().is_empty() {
            continue;
        }
        let envelope = match serde_json::from_str::<JsonRpcMessage>(&line) {
            Ok(envelope) => envelope,
            Err(error) if !line.trim_start().starts_with('{') => {
                inbox.append(InboxItem::NonJson {
                    parse_error: error.to_string(),
                    raw_preview: bounded_utf8_preview(&line, NON_JSON_STDOUT_PREVIEW_BYTES),
                    raw_bytes: line.len(),
                });
                continue;
            }
            Err(error) => {
                append_fatal_decode(&inbox, "unknown", &line, &error.to_string());
                fail_all_waiters(&waiters, "Codex stream contained malformed JSON-RPC");
                inbox.close();
                return;
            }
        };
        match envelope {
            JsonRpcMessage::Response(response) => {
                deliver_response(&waiters, response.id, Ok(response.result));
            }
            JsonRpcMessage::Error(response) => {
                let message = format!(
                    "JSON-RPC error ({}): {}",
                    response.error.code, response.error.message
                );
                deliver_response(&waiters, response.id, Err(HarnessError::Transport(message)));
            }
            JsonRpcMessage::Notification(JsonRpcNotification { method, params }) => {
                match Notification::from_envelope(&method, params) {
                    Ok(notification) => {
                        inbox.append(InboxItem::Message(Box::new(ServerMessage::Notification(
                            notification,
                        ))));
                    }
                    Err(error) => {
                        append_fatal_decode(&inbox, &method, &line, &error.to_string());
                        fail_all_waiters(&waiters, "Codex notification decode failed");
                        inbox.close();
                        return;
                    }
                }
            }
            JsonRpcMessage::Request(JsonRpcRequest { id, method, params }) => {
                // Preserve the current MCP envelope: the generated enum omits top-level
                // routing fields and narrows standard schemas, losing validation keywords.
                let decoded = if method == "mcpServer/elicitation/request" {
                    Ok(ServerRequest::Unknown {
                        method: method.clone(),
                        params,
                    })
                } else {
                    ServerRequest::from_envelope(&method, params)
                };
                match decoded {
                    Ok(request) => {
                        inbox.append(InboxItem::Message(Box::new(ServerMessage::Request {
                            id,
                            request,
                        })));
                    }
                    Err(error) => {
                        append_fatal_decode(&inbox, &method, &line, &error.to_string());
                        fail_all_waiters(&waiters, "Codex request decode failed");
                        inbox.close();
                        return;
                    }
                }
            }
        }
    }
}

async fn write_stdin<W>(writer: W, mut frames: mpsc::Receiver<Frame>, waiters: Waiters)
where
    W: AsyncWrite + Unpin,
{
    let mut writer = BufWriter::new(writer);
    while let Some(frame) = frames.recv().await {
        let result = async {
            writer.write_all(frame.line.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await
        }
        .await
        .map_err(|error| HarnessError::Transport(error.to_string()));
        if result.is_ok()
            && let Some(state) = &frame.state
        {
            state.store(2, Ordering::Release);
        }
        if let Some(written) = frame.written {
            let _ = written.send(result.clone());
        }
        if let Err(error) = result {
            error!(description = %frame.description, %error, "failed to write Codex JSON-RPC frame");
            fail_all_waiters(&waiters, &error.to_string());
            return;
        }
    }
}

fn append_fatal_decode(inbox: &EventLog<InboxItem>, method: &str, line: &str, error: &str) {
    let raw_preview = bounded_utf8_preview(line, NON_JSON_STDOUT_PREVIEW_BYTES);
    inbox.append(InboxItem::Fatal(format!(
        "Codex JSON-RPC deserialization error for method {method}: {error} \
         (raw_bytes: {}, raw_preview: {raw_preview:?})",
        line.len()
    )));
}

fn deliver_response(waiters: &Waiters, id: RequestId, result: Result<Value, HarnessError>) {
    if let Some(waiter) = lock_waiters(waiters).remove(&id) {
        let _ = waiter.send(result);
    } else {
        debug!(request_id = %id, "dropping Codex response without a pending request");
    }
}

fn fail_all_waiters(waiters: &Waiters, message: &str) {
    let pending = std::mem::take(&mut *lock_waiters(waiters));
    for (_, waiter) in pending {
        let _ = waiter.send(Err(HarnessError::Transport(message.to_owned())));
    }
}

fn lock_waiters(waiters: &Waiters) -> MutexGuard<'_, HashMap<RequestId, Waiter>> {
    lock_mutex(waiters)
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            warn!("Codex response-correlation lock was poisoned; recovering");
            poisoned.into_inner()
        }
    }
}

fn serialize_line<T: Serialize>(value: &T) -> Result<String, HarnessError> {
    let line =
        serde_json::to_string(value).map_err(|error| HarnessError::Protocol(error.to_string()))?;
    if line.contains(['\r', '\n']) {
        return Err(HarnessError::Protocol(
            "raw app-server frame contains an embedded line break".to_owned(),
        ));
    }
    Ok(line)
}

fn trim_line_ending(line: &mut String) {
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
}

fn transport_closed() -> HarnessError {
    HarnessError::Transport("Codex transport closed".to_owned())
}

fn drain_stderr(stderr: tokio::process::ChildStderr) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => return,
                Ok(_) => {
                    let line = strip_ansi(&line);
                    let line = line.trim_end_matches(['\n', '\r']);
                    if line.contains(" ERROR ") {
                        error!(target: "codex_codes::stderr", "{line}");
                    } else if line.contains(" WARN ") {
                        warn!(target: "codex_codes::stderr", "{line}");
                    } else if line.contains(" DEBUG ") {
                        debug!(target: "codex_codes::stderr", "{line}");
                    } else {
                        trace!(target: "codex_codes::stderr", "{line}");
                    }
                }
                Err(error) => {
                    debug!(%error, "stopped draining Codex stderr");
                    return;
                }
            }
        }
    })
}

fn strip_ansi(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for control in chars.by_ref() {
                if !(control.is_ascii_digit() || control == ';') {
                    break;
                }
            }
        } else {
            output.push(character);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use std::pin::Pin;
    use std::sync::atomic::AtomicBool;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf, duplex, split};
    use tokio::time::{Duration, timeout};

    struct Peer {
        reader: BufReader<ReadHalf<DuplexStream>>,
        writer: WriteHalf<DuplexStream>,
    }

    struct GatedWriter {
        polled: Arc<AtomicBool>,
    }

    #[derive(Clone)]
    struct CapturedLogWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedLogWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            lock_mutex(&self.0).extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn capture_debug_logs(log: impl FnOnce()) -> String {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer_output = output.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || CapturedLogWriter(writer_output.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, log);
        String::from_utf8(lock_mutex(&output).clone()).unwrap()
    }

    impl AsyncWrite for GatedWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buffer: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.polled.store(true, Ordering::Release);
            Poll::Pending
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn test_transport(limit: usize) -> (StdioTransport, Peer) {
        let (transport_pipe, peer_pipe) = duplex(1024 * 1024);
        let (transport_reader, transport_writer) = split(transport_pipe);
        let (peer_reader, peer_writer) = split(peer_pipe);
        (
            StdioTransport::from_io(transport_reader, transport_writer, None, None, limit),
            Peer {
                reader: BufReader::new(peer_reader),
                writer: peer_writer,
            },
        )
    }

    impl Peer {
        async fn read_json(&mut self) -> Value {
            let mut line = String::new();
            self.reader.read_line(&mut line).await.unwrap();
            serde_json::from_str(&line).unwrap()
        }

        async fn write_json(&mut self, value: Value) {
            self.writer
                .write_all(format!("{value}\n").as_bytes())
                .await
                .unwrap();
        }

        async fn write_raw(&mut self, value: &str) {
            self.writer.write_all(value.as_bytes()).await.unwrap();
            self.writer.write_all(b"\n").await.unwrap();
        }
    }

    fn stub_provider(json: &str) -> Vec<String> {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("cat >/dev/null; printf '%s' '{}'", json),
        ]
    }

    #[tokio::test]
    async fn native_service_can_unblock_client_rpc_and_preserves_other_frames() {
        let (mut transport, mut peer) = test_transport(CODEX_INBOX_RETAIN_LIMIT);
        transport.set_native_services(crate::NativeServiceProviders {
            attestation_command: stub_provider(r#"{"token":"stub-attestation"}"#),
            ..Default::default()
        });
        let client = async {
            assert_eq!(
                transport
                    .request_json("test/request", json!({}))
                    .await
                    .unwrap(),
                json!({"ok": true})
            );
            for expected in ["unknown/one", "unknown/two"] {
                let ServerMessage::Notification(Notification::Unknown { method, .. }) =
                    transport.next_message().await.unwrap().unwrap()
                else {
                    panic!("expected notification")
                };
                assert_eq!(method, expected);
            }
        };
        let server = async {
            let rpc = peer.read_json().await;
            peer.write_json(json!({"method":"unknown/one"})).await;
            peer.write_json(
                json!({"id":"service-1", "method":"attestation/generate", "params":{}}),
            )
            .await;
            let token = peer.read_json().await;
            assert_eq!(
                token,
                json!({"id":"service-1","result":{"token":"stub-attestation"}})
            );
            peer.write_json(json!({"method":"unknown/two"})).await;
            peer.write_json(json!({"id":rpc["id"],"result":{"ok":true}}))
                .await;
        };
        timeout(Duration::from_secs(2), async {
            tokio::join!(client, server);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn external_auth_login_and_refresh_share_provider_and_stay_off_browser_stream() {
        let (mut transport, mut peer) = test_transport(CODEX_INBOX_RETAIN_LIMIT);
        let providers = crate::NativeServiceProviders {
            external_auth_command: stub_provider(
                r#"{"accessToken":"stub-token","chatgptAccountId":"stub-account"}"#,
            ),
            ..Default::default()
        };
        transport.set_native_services(providers.clone());
        let client = async {
            providers.login(&mut transport).await.unwrap();
            let message = transport.next_message().await.unwrap().unwrap();
            assert!(matches!(message, ServerMessage::Notification(_)));
        };
        let server = async {
            let login = peer.read_json().await;
            assert_eq!(login["method"], "account/login/start");
            assert_eq!(
                login["params"],
                json!({"type":"chatgptAuthTokens","accessToken":"stub-token","chatgptAccountId":"stub-account","chatgptPlanType":null})
            );
            peer.write_json(json!({"id":login["id"],"result":{"type":"chatgptAuthTokens"}}))
                .await;
            peer.write_json(json!({"id":42,"method":"account/chatgptAuthTokens/refresh","params":{"reason":"unauthorized","previousAccountId":"stub-account"}})).await;
            let refreshed = peer.read_json().await;
            assert_eq!(
                refreshed,
                json!({"id":42,"result":{"accessToken":"stub-token","chatgptAccountId":"stub-account","chatgptPlanType":null}})
            );
            peer.write_json(json!({"method":"unknown/done"})).await;
        };
        timeout(Duration::from_secs(2), async {
            tokio::join!(client, server);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn external_login_rejection_does_not_echo_native_error_credentials() {
        let (mut transport, mut peer) = test_transport(CODEX_INBOX_RETAIN_LIMIT);
        let providers = crate::NativeServiceProviders {
            external_auth_command: stub_provider(
                r#"{"accessToken":"stub-secret","chatgptAccountId":"stub-account"}"#,
            ),
            ..Default::default()
        };
        let client = providers.login(&mut transport);
        let server = async {
            let login = peer.read_json().await;
            peer.write_json(
                json!({"id":login["id"],"error":{"code":-32000,"message":"rejected stub-secret"}}),
            )
            .await;
        };
        let (result, ()) = tokio::join!(client, server);
        let error = result.unwrap_err().to_string();
        assert!(error.contains("Codex rejected external auth login"));
        assert!(!error.contains("stub-secret"));
    }

    #[tokio::test]
    async fn unconfigured_services_receive_errors_without_browser_actions() {
        let (mut transport, mut peer) = test_transport(CODEX_INBOX_RETAIN_LIMIT);
        let client = transport.next_message();
        let server = async {
            for (id, method, params) in [
                (json!(7), "attestation/generate", json!({})),
                (
                    json!("auth"),
                    "account/chatgptAuthTokens/refresh",
                    json!({"reason":"unauthorized"}),
                ),
            ] {
                peer.write_json(json!({"id":id,"method":method,"params":params}))
                    .await;
                let error = peer.read_json().await;
                assert_eq!(error["id"], id);
                assert_eq!(error["error"]["code"], -32000);
                assert!(
                    error["error"]["message"]
                        .as_str()
                        .unwrap()
                        .contains("configure a trusted host provider")
                );
            }
            peer.write_json(json!({"method":"unknown/done"})).await;
        };
        timeout(Duration::from_secs(2), async {
            let (message, ()) = tokio::join!(client, server);
            assert!(matches!(
                message.unwrap(),
                Some(ServerMessage::Notification(_))
            ));
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn repeated_read_cancellation_resumes_one_provider_invocation() {
        let directory = tempfile::Builder::new()
            .prefix(".native-service-test-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        let count_path = directory.path().join("invocations");
        let (mut transport, mut peer) = test_transport(CODEX_INBOX_RETAIN_LIMIT);
        transport.set_native_services(crate::NativeServiceProviders {
            attestation_command: vec!["/bin/sh".into(), "-c".into(),
                "cat >/dev/null; printf 'invoked\n' >> \"$1\"; sleep 0.25; printf '%s' '{\"token\":\"stub-once\"}'".into(),
                "provider".into(), count_path.to_string_lossy().into_owned()],
            ..Default::default()
        });
        peer.write_json(json!({"id":"cancelled", "method":"attestation/generate", "params":{}}))
            .await;
        let client = async {
            for _ in 0..60 {
                // This mirrors the instance select loop dropping next_message for timer ticks.
                match timeout(Duration::from_millis(20), transport.next_message()).await {
                    Err(_) => {}
                    Ok(result) => {
                        assert!(result.unwrap().is_some());
                        return;
                    }
                }
            }
            panic!("repeated cancellation starved the provider");
        };
        let server = async {
            assert_eq!(
                peer.read_json().await,
                json!({"id":"cancelled","result":{"token":"stub-once"}})
            );
            peer.write_json(json!({"method":"unknown/done"})).await;
        };
        timeout(Duration::from_secs(2), async {
            tokio::join!(client, server);
        })
        .await
        .unwrap();
        assert!(transport.pending_service.is_none());
        assert_eq!(std::fs::read_to_string(count_path).unwrap(), "invoked\n");
    }

    #[tokio::test]
    async fn cancelling_response_write_does_not_enqueue_a_duplicate_frame() {
        let (writer, receiver) = duplex(1);
        let mut receiver = BufReader::new(receiver);
        let mut transport = StdioTransport::from_pipes(tokio::io::empty(), writer);
        let message = ServerMessage::from_value(
            json!({"id":"one-write","method":"attestation/generate","params":{}}),
        )
        .unwrap();
        assert!(
            timeout(
                Duration::from_millis(20),
                transport.service_inbox_item(InboxItem::Message(Box::new(message)))
            )
            .await
            .is_err()
        );
        for _ in 0..3 {
            assert!(
                timeout(Duration::from_millis(20), transport.next_message())
                    .await
                    .is_err()
            );
        }
        let client = transport.next_message();
        let reader = async {
            let mut line = String::new();
            receiver.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap()["id"],
                "one-write"
            );
        };
        let (message, ()) = tokio::join!(client, reader);
        assert!(message.unwrap().is_none());
        assert!(
            timeout(
                Duration::from_millis(20),
                receiver.read_line(&mut String::new())
            )
            .await
            .is_err()
        );
        assert!(transport.pending_service.is_none());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn shutdown_drops_and_kills_suspended_provider_future() {
        let directory = tempfile::Builder::new()
            .prefix(".native-service-test-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        for explicit_shutdown in [true, false] {
            let pid_path = directory.path().join(format!("pid-{explicit_shutdown}"));
            let (mut transport, mut peer) = test_transport(CODEX_INBOX_RETAIN_LIMIT);
            transport.set_native_services(crate::NativeServiceProviders {
                attestation_command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "printf '%s' \"$$\" > \"$1\"; exec sleep 60".into(),
                    "provider".into(),
                    pid_path.to_string_lossy().into_owned(),
                ],
                ..Default::default()
            });
            peer.write_json(json!({"id":"shutdown","method":"attestation/generate","params":{}}))
                .await;
            timeout(Duration::from_secs(2), async {
                while !pid_path.exists() {
                    assert!(
                        timeout(Duration::from_millis(20), transport.next_message())
                            .await
                            .is_err()
                    );
                }
            })
            .await
            .unwrap();
            let pid = std::fs::read_to_string(&pid_path).unwrap();
            let proc_path = std::path::PathBuf::from(format!("/proc/{pid}"));
            assert!(proc_path.exists());
            assert!(transport.pending_service.is_some());
            if explicit_shutdown {
                timeout(Duration::from_secs(1), transport.shutdown_transport())
                    .await
                    .unwrap()
                    .unwrap();
            } else {
                drop(transport);
            }
            timeout(Duration::from_secs(2), async {
                while proc_path.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("provider must be killed and reaped when its owning transport shuts down");
        }
    }

    #[tokio::test]
    async fn service_response_write_timeout_poison_connection_and_bounds_owner_wait() {
        let mut transport = StdioTransport::from_pipes(
            tokio::io::empty(),
            GatedWriter {
                polled: Arc::new(AtomicBool::new(false)),
            },
        );
        let message = ServerMessage::from_value(
            json!({"id":"blocked-writer","method":"attestation/generate","params":{}}),
        )
        .unwrap();
        let result = timeout(
            Duration::from_secs(2),
            transport.service_inbox_item(InboxItem::Message(Box::new(message))),
        )
        .await
        .unwrap();
        assert!(matches!(result, Err(HarnessError::Timeout(_))));
        assert!(transport.pending_service.is_none());
        assert!(matches!(
            transport.next_message().await,
            Err(CodexStreamError::Fatal(HarnessError::Timeout(_)))
        ));
        assert!(matches!(
            transport.request_json("never/sent", json!({})).await,
            Err(HarnessError::Timeout(_))
        ));
    }

    #[tokio::test]
    async fn deferred_inbox_overflow_remains_fatal_after_rpc_returns() {
        let (mut transport, mut peer) = test_transport(CODEX_INBOX_RETAIN_LIMIT);
        transport
            .deferred_inbox
            .resize(CODEX_INBOX_RETAIN_LIMIT, InboxItem::Eof);
        let client = transport.request_json("test/request", json!({}));
        let server = async {
            peer.read_json().await;
            peer.write_json(json!({"method":"unknown/overflow"})).await;
        };
        let (result, ()) = tokio::join!(client, server);
        assert!(
            matches!(result, Err(HarnessError::Transport(message)) if message.contains("overflowed"))
        );
        assert!(matches!(
            transport.next_message().await,
            Err(CodexStreamError::Fatal(_))
        ));
    }

    #[tokio::test]
    async fn mcp_elicitation_preserves_current_envelope_and_schema() {
        let (mut transport, mut peer) = test_transport(CODEX_INBOX_RETAIN_LIMIT);
        let params = json!({
            "mode":"openai/form", "threadId":"native-child", "turnId":null,
            "serverName":"survey", "message":"Configure", "futureField":{"retained":true},
            "requestedSchema":{"type":"object", "properties":{"count":{"type":"integer", "minimum":2}}, "required":["count"]}
        });
        peer.write_json(
            json!({"id":"form-1", "method":"mcpServer/elicitation/request", "params":params}),
        )
        .await;
        let message = transport.next_message().await.unwrap().unwrap();
        let ServerMessage::Request {
            id,
            request:
                ServerRequest::Unknown {
                    method,
                    params: actual,
                },
        } = message
        else {
            panic!("MCP request must retain raw parameters");
        };
        assert_eq!(id, RequestId::String("form-1".into()));
        assert_eq!(method, "mcpServer/elicitation/request");
        assert_eq!(actual, Some(params));
    }

    #[tokio::test]
    async fn notifications_during_a_request_are_delivered_after_it() {
        let (mut transport, mut peer) = test_transport(CODEX_INBOX_RETAIN_LIMIT);
        let request = transport.request_json("test/request", json!({"value": 1}));
        let server = async {
            let frame = peer.read_json().await;
            assert_eq!(frame["id"], 1);
            peer.write_json(json!({"method":"unknown/one","params":{"n":1}}))
                .await;
            peer.write_json(json!({"method":"unknown/two","params":{"n":2}}))
                .await;
            peer.write_json(json!({"id":1,"result":{"ok":true}})).await;
        };
        let (response, ()) = tokio::join!(request, server);
        assert_eq!(response.unwrap(), json!({"ok": true}));

        for expected in ["unknown/one", "unknown/two"] {
            let message = transport.next_message().await.unwrap().unwrap();
            let ServerMessage::Notification(Notification::Unknown { method, .. }) = message else {
                panic!("expected unknown notification");
            };
            assert_eq!(method, expected);
        }
    }

    #[tokio::test]
    async fn responses_are_correlated_and_notifications_keep_their_order() {
        let (mut transport, mut peer) = test_transport(CODEX_INBOX_RETAIN_LIMIT);
        let writer = transport.writer_tx.as_ref().unwrap().clone();
        let waiters = transport.waiters.clone();
        let next_id = transport.next_id.clone();
        let first = request_json(
            Some(writer.clone()),
            waiters.clone(),
            next_id.clone(),
            "request/first",
            json!({}),
            None,
        );
        let second = request_json(
            Some(writer),
            waiters,
            next_id,
            "request/second",
            json!({}),
            None,
        );
        let server = async {
            let first_frame = peer.read_json().await;
            let second_frame = peer.read_json().await;
            let ids = HashMap::from([
                (
                    first_frame["method"].as_str().unwrap().to_owned(),
                    first_frame["id"].clone(),
                ),
                (
                    second_frame["method"].as_str().unwrap().to_owned(),
                    second_frame["id"].clone(),
                ),
            ]);
            peer.write_json(json!({"method":"unknown/one"})).await;
            peer.write_json(json!({"id":ids["request/second"],"result":"second"}))
                .await;
            peer.write_json(json!({"method":"unknown/two"})).await;
            peer.write_json(json!({"id":ids["request/first"],"result":"first"}))
                .await;
        };
        let (first, second, ()) = tokio::join!(first, second, server);
        assert_eq!(first.unwrap(), json!("first"));
        assert_eq!(second.unwrap(), json!("second"));
        for expected in ["unknown/one", "unknown/two"] {
            let ServerMessage::Notification(Notification::Unknown { method, .. }) =
                transport.next_message().await.unwrap().unwrap()
            else {
                panic!("expected unknown notification");
            };
            assert_eq!(method, expected);
        }
    }

    #[tokio::test]
    async fn writer_never_interleaves_frames() {
        let (transport, mut peer) = test_transport(CODEX_INBOX_RETAIN_LIMIT);
        let sender = transport.writer_tx.as_ref().unwrap().clone();
        let mut tasks = Vec::new();
        for index in 0..5 {
            let request_sender = sender.clone();
            let waiters = transport.waiters.clone();
            let next_id = transport.next_id.clone();
            tasks.push(tokio::spawn(async move {
                request_json(
                    Some(request_sender),
                    waiters,
                    next_id,
                    &format!("request/{index}"),
                    json!({"body":"x".repeat(4096)}),
                    None,
                )
                .await
            }));
            let response_sender = sender.clone();
            tasks.push(tokio::spawn(async move {
                send_frame(
                    Some(response_sender),
                    &JsonRpcResponse {
                        id: RequestId::Integer(100 + index),
                        result: json!({"body":"x".repeat(4096)}),
                    },
                    format!("response {index}"),
                )
                .await
                .map(|()| Value::Null)
            }));
        }
        let mut request_count = 0;
        let mut response_count = 0;
        while request_count + response_count < 10 {
            let frame = peer.read_json().await;
            if frame.get("method").is_some() {
                request_count += 1;
                peer.write_json(json!({"id":frame["id"],"result":null}))
                    .await;
            } else {
                response_count += 1;
                assert!(frame.get("result").is_some());
            }
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!((request_count, response_count), (5, 5));
    }

    #[tokio::test]
    async fn a_late_response_for_a_timed_out_request_is_dropped() {
        let (mut transport, mut peer) = test_transport(CODEX_INBOX_RETAIN_LIMIT);
        let first = timeout(
            Duration::from_millis(20),
            transport.request_json("first", json!({})),
        );
        let read_first = peer.read_json();
        let (timed_out, first_frame) = tokio::join!(first, read_first);
        assert!(timed_out.is_err());
        assert_eq!(first_frame["id"], 1);
        peer.write_json(json!({"id":1,"result":"late"})).await;

        let second = transport.request_json("second", json!({}));
        let server = async {
            let frame = peer.read_json().await;
            assert_eq!(frame["id"], 2);
            peer.write_json(json!({"id":2,"result":"current"})).await;
        };
        let (response, ()) = tokio::join!(second, server);
        assert_eq!(response.unwrap(), json!("current"));
    }

    #[test]
    fn a_late_response_is_logged_exactly_once() {
        let waiters = Arc::new(Mutex::new(HashMap::new()));
        let output = capture_debug_logs(|| {
            deliver_response(&waiters, RequestId::Integer(7), Ok(json!("late")));
        });
        assert_eq!(
            output
                .matches("dropping Codex response without a pending request")
                .count(),
            1
        );
        assert!(output.contains("request_id=7"), "{output}");
    }

    #[tokio::test]
    async fn timeout_records_written_and_not_queued_states() {
        let (transport, mut peer) = test_transport(CODEX_INBOX_RETAIN_LIMIT);
        let written_states = Arc::new(Mutex::new(Vec::new()));
        let request = timeout(
            Duration::from_millis(20),
            request_json(
                transport.writer_tx.as_ref().cloned(),
                transport.waiters.clone(),
                transport.next_id.clone(),
                "written",
                json!({}),
                Some(written_states.clone()),
            ),
        );
        let read = peer.read_json();
        let (timed_out, _) = tokio::join!(request, read);
        assert!(timed_out.is_err());
        assert_eq!(*lock_mutex(&written_states), vec![2]);

        let (reader, _peer) = duplex(64);
        let polled = Arc::new(AtomicBool::new(false));
        let transport = StdioTransport::from_io(
            reader,
            GatedWriter {
                polled: polled.clone(),
            },
            None,
            None,
            CODEX_INBOX_RETAIN_LIMIT,
        );
        let sender = transport.writer_tx.as_ref().unwrap().clone();
        sender
            .send(Frame {
                line: "{}".into(),
                description: "blocked frame".into(),
                written: None,
                state: None,
            })
            .await
            .unwrap();
        while !polled.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        for index in 0..WRITER_QUEUE_CAPACITY {
            sender
                .send(Frame {
                    line: "{}".into(),
                    description: format!("queued frame {index}"),
                    written: None,
                    state: None,
                })
                .await
                .unwrap();
        }
        let unqueued_states = Arc::new(Mutex::new(Vec::new()));
        assert!(
            timeout(
                Duration::from_millis(20),
                request_json(
                    Some(sender),
                    transport.waiters.clone(),
                    transport.next_id.clone(),
                    "not-queued",
                    json!({}),
                    Some(unqueued_states.clone()),
                ),
            )
            .await
            .is_err()
        );
        assert_eq!(*lock_mutex(&unqueued_states), vec![0]);
    }

    #[tokio::test]
    async fn eof_drains_the_inbox_then_reports_none() {
        let (mut transport, mut peer) = test_transport(CODEX_INBOX_RETAIN_LIMIT);
        let pending = request_json(
            transport.writer_tx.as_ref().cloned(),
            transport.waiters.clone(),
            transport.next_id.clone(),
            "pending",
            json!({}),
            None,
        );
        let establish_pending = async {
            peer.read_json().await;
            peer.write_json(json!({"method":"unknown/one"})).await;
            peer.write_json(json!({"method":"unknown/two"})).await;
            peer.writer.shutdown().await.unwrap();
        };
        let (pending_result, ()) = tokio::join!(pending, establish_pending);

        assert!(transport.next_message().await.unwrap().is_some());
        assert!(transport.next_message().await.unwrap().is_some());
        assert!(transport.next_message().await.unwrap().is_none());
        assert!(matches!(
            pending_result,
            Err(HarnessError::Transport(message)) if message == "Codex stream closed"
        ));
    }

    #[tokio::test]
    async fn an_oversized_frame_is_fatal() {
        let (mut transport, mut peer) = test_transport(CODEX_INBOX_RETAIN_LIMIT);
        let (waiter_tx, waiter_rx) = oneshot::channel();
        lock_waiters(&transport.waiters).insert(RequestId::Integer(7), waiter_tx);

        let mut frame = vec![b'x'; CODEX_MAX_FRAME_BYTES];
        frame.push(b'\n');
        peer.writer.write_all(&frame).await.unwrap();

        assert!(matches!(
            waiter_rx.await.unwrap(),
            Err(HarnessError::Transport(message))
                if message == "Codex stream produced an oversized frame"
        ));
        assert!(matches!(
            transport.next_message().await,
            Err(CodexStreamError::Fatal(HarnessError::Transport(message)))
                if message == format!(
                    "Codex stdout frame exceeded {CODEX_MAX_FRAME_BYTES} bytes"
                )
        ));
        assert!(transport.next_message().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_frame_at_the_limit_is_accepted() {
        let (mut transport, mut peer) = test_transport(CODEX_INBOX_RETAIN_LIMIT);
        let mut frame = vec![b'x'; CODEX_MAX_FRAME_BYTES - 1];
        frame.push(b'\n');
        peer.writer.write_all(&frame).await.unwrap();

        let Err(CodexStreamError::NonJsonStdout { raw_bytes, .. }) = transport.next_message().await
        else {
            panic!("expected an accepted non-JSON frame");
        };
        assert_eq!(raw_bytes, CODEX_MAX_FRAME_BYTES - 1);
    }

    #[tokio::test]
    async fn non_json_is_recoverable_and_valid_message_precedes_fatal_garbage() {
        let (mut transport, mut peer) = test_transport(CODEX_INBOX_RETAIN_LIMIT);
        let raw_non_json = format!("{}é", "x".repeat(NON_JSON_STDOUT_PREVIEW_BYTES - 1));
        peer.write_raw("   ").await;
        peer.write_raw(&raw_non_json).await;
        peer.write_json(json!({"method":"unknown/valid"})).await;
        peer.write_raw(r#"{"method":"turn/completed""#).await;

        let Err(CodexStreamError::NonJsonStdout {
            raw_preview,
            raw_bytes,
            ..
        }) = transport.next_message().await
        else {
            panic!("expected recoverable non-JSON line");
        };
        assert_eq!(raw_preview.len(), NON_JSON_STDOUT_PREVIEW_BYTES - 1);
        assert_eq!(raw_bytes, raw_non_json.len());
        assert!(transport.next_message().await.unwrap().is_some());
        assert!(matches!(
            transport.next_message().await,
            Err(CodexStreamError::Fatal(HarnessError::Transport(_)))
        ));
    }

    #[tokio::test]
    async fn parseable_invalid_envelope_and_typed_decode_error_are_fatal() {
        let (mut invalid_envelope, mut peer) = test_transport(CODEX_INBOX_RETAIN_LIMIT);
        peer.write_raw(r#"{"unexpected":true}"#).await;
        assert!(matches!(
            invalid_envelope.next_message().await,
            Err(CodexStreamError::Fatal(HarnessError::Transport(_)))
        ));

        let (mut typed_error, mut peer) = test_transport(CODEX_INBOX_RETAIN_LIMIT);
        peer.write_json(json!({"method":"turn/completed","params":{"unexpected":true}}))
            .await;
        assert!(matches!(
            typed_error.next_message().await,
            Err(CodexStreamError::Fatal(HarnessError::Transport(_)))
        ));
    }

    #[tokio::test]
    async fn inbox_overflow_is_fatal_with_a_count() {
        let (mut transport, mut peer) = test_transport(2);
        let (barrier_tx, barrier_rx) = oneshot::channel();
        lock_waiters(&transport.waiters).insert(RequestId::Integer(99), barrier_tx);
        for index in 0..5 {
            peer.write_json(json!({"method":"unknown/event","params":{"index":index}}))
                .await;
        }
        peer.write_json(json!({"id":99,"result":null})).await;
        barrier_rx.await.unwrap().unwrap();
        let Err(CodexStreamError::Fatal(HarnessError::Transport(message))) =
            transport.next_message().await
        else {
            panic!("expected fatal overflow");
        };
        assert!(message.contains("3 frames dropped"), "{message}");
    }

    #[tokio::test]
    async fn initialize_gets_id_one_and_initialized_has_no_id() {
        let (mut transport, mut peer) = test_transport(CODEX_INBOX_RETAIN_LIMIT);
        let initialize = transport.request_json("initialize", json!({}));
        let server = async {
            let frame = peer.read_json().await;
            assert_eq!(frame["id"], 1);
            peer.write_json(json!({"id":1,"result":{}})).await;
        };
        let (response, ()) = tokio::join!(initialize, server);
        response.unwrap();
        transport.send_notification("initialized").await.unwrap();
        let frame = peer.read_json().await;
        assert_eq!(frame, json!({"method":"initialized"}));
    }

    #[tokio::test]
    async fn shutdown_fails_pending_waiters_and_closes_the_inbox() {
        let (transport, _peer) = test_transport(CODEX_INBOX_RETAIN_LIMIT);
        let mut inbox_reader = transport.inbox.reader();
        let (waiter_tx, waiter_rx) = oneshot::channel();
        lock_waiters(&transport.waiters).insert(RequestId::Integer(7), waiter_tx);
        transport.shutdown_transport().await.unwrap();
        assert!(matches!(
            waiter_rx.await.unwrap(),
            Err(HarnessError::Transport(message)) if message == "Codex transport shut down"
        ));
        assert!(matches!(
            inbox_reader.recv().await,
            Err(EventStreamError::Closed)
        ));
    }

    #[test]
    fn stderr_ansi_stripping_preserves_unicode() {
        assert_eq!(strip_ansi("\u{1b}[32mréussi\u{1b}[0m"), "réussi");
    }

    #[tokio::test]
    async fn from_pipes_constructs_a_transport() {
        let (reader, _) = duplex(64);
        let (writer, _) = duplex(64);
        let _transport = StdioTransport::from_pipes(reader, writer);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_kills_a_real_child_process() {
        use std::process::Stdio;

        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "while :; do sleep 1; done"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let transport = StdioTransport::from_io(
            stdout,
            stdin,
            Some(child),
            Some(drain_stderr(stderr)),
            CODEX_INBOX_RETAIN_LIMIT,
        );
        timeout(Duration::from_secs(1), transport.shutdown_transport())
            .await
            .unwrap()
            .unwrap();
    }
}
