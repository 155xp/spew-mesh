//! The daemon's side of the control socket (6.1, 6.2): accept connections from this user only, read one JSON-RPC
//! request per line, answer each as soon as it is done (requests overlap), and tell the handler when a
//! connection closes so waits end early and sessions can detach (6.4, 7.5).

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};

use super::place::same_user;
use super::protocol::{Inbound, MAX_LINE, RpcError, notification_line, parse_line, response_line};

/// One client connection, as a request handler sees it.
#[derive(Debug, Clone)]
pub struct Conn {
    id: u64,
    closed: watch::Receiver<bool>,
    out: mpsc::UnboundedSender<String>,
}

impl Conn {
    /// A number that names this connection for the daemon's life.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Whether the client has gone.
    pub fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }

    /// Resolves when the client goes. A wait selects on this so it ends early (7.5).
    pub async fn closed(&self) {
        let mut rx = self.closed.clone();
        let _ = rx.wait_for(|closed| *closed).await;
    }

    /// Sends a notification on this connection.
    pub fn notify(&self, method: &str, params: &Value) {
        let _ = self.out.send(notification_line(method, params));
    }

    /// A connection that is never closed and drops what it is sent, for calling a handler directly in tests.
    pub fn detached(id: u64) -> (Conn, watch::Sender<bool>) {
        let (tx, rx) = watch::channel(false);
        let (out, _) = mpsc::unbounded_channel();
        (
            Conn {
                id,
                closed: rx,
                out,
            },
            tx,
        )
    }
}

/// What the daemon does with requests.
pub trait Handler: Send + Sync + 'static {
    /// Answers one request.
    fn handle(
        &self,
        conn: Conn,
        method: String,
        params: Value,
    ) -> impl Future<Output = Result<Value, RpcError>> + Send;

    /// A connection closed.
    fn closed(&self, conn: u64);
}

static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

/// Accepts connections until `shutdown` turns true. Connections from other users are dropped at once (6.1).
pub async fn serve<H: Handler>(
    listener: UnixListener,
    handler: Arc<H>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = shutdown.wait_for(|stop| *stop) => return,
        };
        let stream = match accepted {
            Ok((stream, _)) => stream,
            Err(e) => {
                tracing::warn!(error = %e, "control socket accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        match same_user(&stream) {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!("dropped a control connection from another user");
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not read a control connection's credentials");
                continue;
            }
        }
        let id = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(connection(stream, id, handler.clone(), shutdown.clone()));
    }
}

async fn connection<H: Handler>(
    stream: UnixStream,
    id: u64,
    handler: Arc<H>,
    mut shutdown: watch::Receiver<bool>,
) {
    let (read, mut write) = stream.into_split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let (closed_tx, closed_rx) = watch::channel(false);
    let writer = tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            if write.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let conn = Conn {
        id,
        closed: closed_rx,
        out: out_tx.clone(),
    };
    let mut reader = BufReader::new(read);
    loop {
        let line = tokio::select! {
            line = read_line(&mut reader) => line,
            _ = shutdown.wait_for(|stop| *stop) => break,
        };
        let line = match line {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(LineError::TooLong) => {
                let err =
                    RpcError::parse_error("the line is longer than the control protocol allows");
                let _ = out_tx.send(response_line(None, &Err(err)));
                break;
            }
            Err(LineError::Io) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        match parse_line(&line) {
            Ok(Inbound::Request {
                id: rid,
                method,
                params,
            }) => {
                let handler = handler.clone();
                let conn = conn.clone();
                let out = out_tx.clone();
                tokio::spawn(async move {
                    let outcome = handler.handle(conn, method, params).await;
                    let _ = out.send(response_line(Some(&rid), &outcome));
                });
            }
            // Clients send no notifications or responses the daemon needs.
            Ok(Inbound::Notification { .. } | Inbound::Response { .. }) => {}
            Err((rid, err)) => {
                let _ = out_tx.send(response_line(rid.as_ref(), &Err(err)));
            }
        }
    }
    let _ = closed_tx.send(true);
    handler.closed(id);
    drop(conn);
    drop(out_tx);
    // In-flight requests still hold senders; the writer ends when they finish or the socket breaks.
    let _ = writer.await;
}

#[derive(Debug)]
pub(crate) enum LineError {
    TooLong,
    Io,
}

/// Reads one line of at most [`MAX_LINE`] bytes, without its newline. `Ok(None)` at a clean end of stream.
pub(crate) async fn read_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
) -> Result<Option<String>, LineError> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(|_| LineError::Io)?;
        if available.is_empty() {
            return if buf.is_empty() {
                Ok(None)
            } else {
                Ok(Some(String::from_utf8(buf).map_err(|_| LineError::Io)?))
            };
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            if buf.len() + pos > MAX_LINE {
                return Err(LineError::TooLong);
            }
            buf.extend_from_slice(&available[..pos]);
            reader.consume(pos + 1);
            return String::from_utf8(buf).map(Some).map_err(|_| LineError::Io);
        }
        let n = available.len();
        if buf.len() + n > MAX_LINE {
            return Err(LineError::TooLong);
        }
        buf.extend_from_slice(available);
        reader.consume(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::place::bind_private;
    use std::time::Duration;

    struct Echo {
        closed: std::sync::Mutex<Vec<u64>>,
    }

    impl Handler for Echo {
        async fn handle(
            &self,
            conn: Conn,
            method: String,
            params: Value,
        ) -> Result<Value, RpcError> {
            match method.as_str() {
                "echo" => Ok(params),
                "slow" => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    Ok(Value::String("slow".into()))
                }
                "wait" => {
                    conn.closed().await;
                    Ok(Value::Null)
                }
                "notify" => {
                    conn.notify("note", &serde_json::json!({"n": 1}));
                    Ok(Value::Bool(true))
                }
                other => Err(RpcError::unknown_method(other)),
            }
        }

        fn closed(&self, conn: u64) {
            self.closed.lock().unwrap().push(conn);
        }
    }

    async fn lines(
        stream: UnixStream,
    ) -> (
        tokio::net::unix::OwnedWriteHalf,
        mpsc::UnboundedReceiver<String>,
    ) {
        let (read, write) = stream.into_split();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut reader = BufReader::new(read);
            while let Ok(Some(line)) = read_line(&mut reader).await {
                let _ = tx.send(line);
            }
        });
        (write, rx)
    }

    #[tokio::test]
    async fn requests_overlap_and_errors_are_answered() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("c.sock");
        let listener = bind_private(&path).unwrap();
        let handler = Arc::new(Echo {
            closed: Default::default(),
        });
        let (stop_tx, stop_rx) = watch::channel(false);
        tokio::spawn(serve(listener, handler.clone(), stop_rx));
        let stream = UnixStream::connect(&path).await.unwrap();
        let (mut write, mut rx) = lines(stream).await;
        write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"slow\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"echo\",\"params\":{\"a\":1}}\nnot json\n{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"nope\"}\n{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"notify\"}\n")
            .await
            .unwrap();
        let mut got = Vec::new();
        for _ in 0..6 {
            let line = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            got.push(serde_json::from_str::<Value>(&line).unwrap());
        }
        // The fast request is answered before the slow one that came first.
        let ids: Vec<Value> = got.iter().map(|v| v["id"].clone()).collect();
        let pos = |id: i64| ids.iter().position(|v| v == &Value::from(id)).unwrap();
        assert!(pos(2) < pos(1));
        assert!(
            got.iter()
                .any(|v| v["error"]["code"] == -32700 && v["id"].is_null())
        );
        assert!(
            got.iter()
                .any(|v| v["id"] == 3 && v["error"]["code"] == -32601)
        );
        assert!(got.iter().any(|v| v["method"] == "note"));
        assert_eq!(got[pos(2)]["result"]["a"], 1);
        drop(write);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(handler.closed.lock().unwrap().len(), 1);
        stop_tx.send(true).unwrap();
    }

    #[tokio::test]
    async fn closing_ends_waits() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("c.sock");
        let listener = bind_private(&path).unwrap();
        let handler = Arc::new(Echo {
            closed: Default::default(),
        });
        let (_stop_tx, stop_rx) = watch::channel(false);
        tokio::spawn(serve(listener, handler.clone(), stop_rx));
        let mut stream = UnixStream::connect(&path).await.unwrap();
        stream
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"wait\"}\n")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(stream);
        for _ in 0..50 {
            if !handler.closed.lock().unwrap().is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the handler never heard the connection close");
    }

    #[tokio::test]
    async fn long_lines_end_the_connection() {
        let (a, b) = tokio::io::duplex(1 << 16);
        let mut reader = BufReader::new(b);
        let writer = tokio::spawn(async move {
            let mut a = a;
            let chunk = vec![b'x'; 1 << 15];
            for _ in 0..40 {
                if a.write_all(&chunk).await.is_err() {
                    break;
                }
            }
        });
        assert!(matches!(
            read_line(&mut reader).await,
            Err(LineError::TooLong)
        ));
        drop(reader);
        let _ = writer.await;
    }
}
