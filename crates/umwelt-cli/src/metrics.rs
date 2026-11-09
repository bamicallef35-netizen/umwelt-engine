//! `umwelt /metrics` — live mission counters served over HTTP.
//!
//! A hand-rolled, std-only Prometheus exposition endpoint. Runs on a
//! single background thread, binds to a caller-chosen address, and
//! writes atomic counters the mission loop updates in real time.
//!
//! ## Design
//!
//! - Every counter is an [`AtomicU64`]. The mission loop increments
//!   with [`Ordering::Relaxed`] — counters are per-mission, never
//!   cross-process, and the scraper reads a snapshot that is always
//!   individually consistent. The emitted text is allowed to show a
//!   slightly stale view of the global state; Prometheus scrapers
//!   tolerate this by design.
//!
//! - Labels are fixed at spawn time (e.g. `kind="run"`, `agent_id="1"`,
//!   `profile="4/64/8"`). They read directly off the ADR-0020 profile
//!   table and the P6.1 mission span, so an operator filtering in
//!   Prometheus sees the same dimensions they see in tracing logs.
//!
//! - No external HTTP crate. We parse only the request line of each
//!   inbound TCP connection, match `GET /metrics`, and write a
//!   fixed response. `/` serves a one-line hint for humans; anything
//!   else returns 404. Keeps the dep surface at zero and the
//!   fingerprint on no_std builds at nothing.
//!
//! - Shutdown is cooperative: the `MetricsServer` flips a stop flag
//!   on drop and wakes the accept loop by self-connecting. The
//!   background thread joins before the server is reaped.

use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

// ── Counters ──────────────────────────────────────────────────────────────

/// Per-mission counters. Shared by [`Arc`] between the mission loop
/// and the metrics server. Every field is monotonically non-decreasing.
#[derive(Debug, Default)]
pub struct MissionMetrics {
    /// Mission ticks elapsed.
    pub ticks: AtomicU64,
    /// Icon-layer events emitted.
    pub icons: AtomicU64,
    /// Index-layer events emitted.
    pub indices: AtomicU64,
    /// Symbol-layer events committed.
    pub symbols: AtomicU64,
    /// SEP frames received by the transport.
    pub recv_frames: AtomicU64,
}

impl MissionMetrics {
    /// Fresh counters, all zero.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

// ── Server ────────────────────────────────────────────────────────────────

/// A live HTTP metrics endpoint. The server runs on one background
/// thread until dropped. Listens on the [`SocketAddr`] returned by
/// [`MetricsServer::addr`].
pub struct MetricsServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MetricsServer {
    /// Bind to `requested` (use `127.0.0.1:0` to let the OS pick) and
    /// spawn the handler thread. `labels` are zipped onto every
    /// counter line — pass `(kind, "run")`, `(agent_id, "1")`, etc.
    pub fn spawn(
        requested: SocketAddr,
        metrics: Arc<MissionMetrics>,
        labels: Vec<(String, String)>,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(requested)?;
        let addr = listener.local_addr()?;
        // 200 ms read timeout so a stuck client can't pin the handler.
        listener.set_nonblocking(false)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_inner = stop.clone();
        let labels = Arc::new(labels);
        let handle = thread::spawn(move || {
            Self::accept_loop(listener, metrics, stop_inner, labels);
        });
        Ok(Self {
            addr,
            stop,
            handle: Some(handle),
        })
    }

    /// Address the server is bound to. Useful after binding `:0`.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    fn accept_loop(
        listener: TcpListener,
        metrics: Arc<MissionMetrics>,
        stop: Arc<AtomicBool>,
        labels: Arc<Vec<(String, String)>>,
    ) {
        for stream in listener.incoming() {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            // Each connection is handled serially. Prometheus scrapers
            // open one short request at a time, so a handler thread
            // pool would be overkill.
            let _ = handle_request(stream, &metrics, &labels);
        }
    }
}

impl Drop for MetricsServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop by self-connecting. Ignore all errors —
        // if the connect fails the listener is already gone.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(200));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ── Request handler ───────────────────────────────────────────────────────

fn handle_request(
    mut stream: TcpStream,
    metrics: &MissionMetrics,
    labels: &[(String, String)],
) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    // Drain remaining headers so the client sees EOF cleanly; we don't
    // care what they say.
    let mut discard = String::new();
    while reader.read_line(&mut discard)? > 0 {
        if discard == "\r\n" || discard == "\n" {
            break;
        }
        discard.clear();
    }

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    let (method, path) = match (parts.first(), parts.get(1)) {
        (Some(m), Some(p)) => (*m, *p),
        _ => {
            return write_status(
                &mut stream,
                400,
                "Bad Request",
                "text/plain",
                b"bad request",
            );
        }
    };

    match (method, path) {
        ("GET", "/metrics") => {
            let body = render_prometheus(metrics, labels);
            write_status(
                &mut stream,
                200,
                "OK",
                "text/plain; version=0.0.4; charset=utf-8",
                body.as_bytes(),
            )
        }
        ("GET", "/") => write_status(
            &mut stream,
            200,
            "OK",
            "text/plain; charset=utf-8",
            b"umwelt metrics; try /metrics\n",
        ),
        ("GET", _) => write_status(&mut stream, 404, "Not Found", "text/plain", b"not found\n"),
        _ => write_status(
            &mut stream,
            405,
            "Method Not Allowed",
            "text/plain",
            b"method not allowed\n",
        ),
    }
}

fn write_status(
    stream: &mut TcpStream,
    code: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    // `access-control-allow-origin: *` lets a browser page (P6.4)
    // fetch /metrics when it is served off `file://` or a different
    // port. Prometheus scrapers ignore the header, so
    // this is unconditionally safe.
    write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\n\
         content-type: {content_type}\r\n\
         content-length: {}\r\n\
         access-control-allow-origin: *\r\n\
         connection: close\r\n\
         \r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

// ── Prometheus rendering ──────────────────────────────────────────────────

fn render_prometheus(metrics: &MissionMetrics, labels: &[(String, String)]) -> String {
    let label_prefix = format_base_labels(labels);
    let mut out = String::with_capacity(1024);
    out.push_str("# HELP umwelt_mission_ticks Mission ticks elapsed.\n");
    out.push_str("# TYPE umwelt_mission_ticks counter\n");
    out.push_str(&format!(
        "umwelt_mission_ticks{{{label_prefix}}} {}\n",
        metrics.ticks.load(Ordering::Relaxed)
    ));
    out.push_str("# HELP umwelt_events_total Pipeline events emitted by layer.\n");
    out.push_str("# TYPE umwelt_events_total counter\n");
    for (layer, v) in [
        ("icon", metrics.icons.load(Ordering::Relaxed)),
        ("index", metrics.indices.load(Ordering::Relaxed)),
        ("symbol", metrics.symbols.load(Ordering::Relaxed)),
    ] {
        let line_labels = label_with(&label_prefix, "layer", layer);
        out.push_str(&format!("umwelt_events_total{{{line_labels}}} {v}\n"));
    }
    out.push_str("# HELP umwelt_recv_frames_total SEP frames received by the transport.\n");
    out.push_str("# TYPE umwelt_recv_frames_total counter\n");
    out.push_str(&format!(
        "umwelt_recv_frames_total{{{label_prefix}}} {}\n",
        metrics.recv_frames.load(Ordering::Relaxed)
    ));
    out
}

fn format_base_labels(labels: &[(String, String)]) -> String {
    labels
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape_label_value(v)))
        .collect::<Vec<_>>()
        .join(",")
}

fn label_with(base: &str, extra_key: &str, extra_val: &str) -> String {
    let extra = format!("{extra_key}=\"{}\"", escape_label_value(extra_val));
    if base.is_empty() {
        extra
    } else {
        format!("{base},{extra}")
    }
}

fn escape_label_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn labels() -> Vec<(String, String)> {
        vec![
            ("kind".into(), "run".into()),
            ("agent_id".into(), "1".into()),
            ("profile".into(), "4/64/8".into()),
        ]
    }

    #[test]
    fn prometheus_text_shape_is_stable() {
        let m = MissionMetrics::new();
        m.ticks.store(42, Ordering::Relaxed);
        m.icons.store(10, Ordering::Relaxed);
        m.indices.store(3, Ordering::Relaxed);
        m.symbols.store(1, Ordering::Relaxed);
        m.recv_frames.store(7, Ordering::Relaxed);
        let text = render_prometheus(&m, &labels());
        assert!(text
            .contains("umwelt_mission_ticks{kind=\"run\",agent_id=\"1\",profile=\"4/64/8\"} 42"));
        assert!(text.contains(
            "umwelt_events_total{kind=\"run\",agent_id=\"1\",profile=\"4/64/8\",layer=\"icon\"} 10"
        ));
        assert!(text.contains(
            "umwelt_events_total{kind=\"run\",agent_id=\"1\",profile=\"4/64/8\",layer=\"symbol\"} 1"
        ));
        assert!(text.contains("# HELP umwelt_mission_ticks"));
        assert!(text.contains("# TYPE umwelt_events_total counter"));
    }

    #[test]
    fn empty_labels_still_render() {
        let m = MissionMetrics::new();
        let text = render_prometheus(&m, &[]);
        assert!(text.contains("umwelt_mission_ticks{} 0"));
        assert!(text.contains("umwelt_events_total{layer=\"icon\"} 0"));
    }

    #[test]
    fn label_escaping_handles_quotes_and_slashes() {
        assert_eq!(escape_label_value(r#"a"b"#), "a\\\"b");
        assert_eq!(escape_label_value("a\\b"), "a\\\\b");
        assert_eq!(escape_label_value("a\nb"), "a\\nb");
    }

    #[test]
    fn server_responds_to_metrics_request() {
        let m = MissionMetrics::new();
        m.icons.store(5, Ordering::Relaxed);
        let server =
            MetricsServer::spawn("127.0.0.1:0".parse().unwrap(), m.clone(), labels()).unwrap();
        let addr = server.addr();

        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nhost: localhost\r\n\r\n")
            .unwrap();
        let mut body = String::new();
        stream.read_to_string(&mut body).unwrap();
        assert!(body.starts_with("HTTP/1.1 200 OK"));
        assert!(body.contains("umwelt_events_total"));
        assert!(body.contains("layer=\"icon\"} 5"));
    }

    #[test]
    fn server_returns_404_on_unknown_path() {
        let m = MissionMetrics::new();
        let server = MetricsServer::spawn("127.0.0.1:0".parse().unwrap(), m, labels()).unwrap();
        let mut stream =
            TcpStream::connect_timeout(&server.addr(), Duration::from_secs(2)).unwrap();
        stream
            .write_all(b"GET /does-not-exist HTTP/1.1\r\nhost: localhost\r\n\r\n")
            .unwrap();
        let mut body = String::new();
        stream.read_to_string(&mut body).unwrap();
        assert!(body.starts_with("HTTP/1.1 404"));
    }

    #[test]
    fn server_returns_405_on_non_get() {
        let m = MissionMetrics::new();
        let server = MetricsServer::spawn("127.0.0.1:0".parse().unwrap(), m, labels()).unwrap();
        let mut stream =
            TcpStream::connect_timeout(&server.addr(), Duration::from_secs(2)).unwrap();
        stream
            .write_all(b"POST /metrics HTTP/1.1\r\nhost: localhost\r\n\r\n")
            .unwrap();
        let mut body = String::new();
        stream.read_to_string(&mut body).unwrap();
        assert!(body.starts_with("HTTP/1.1 405"));
    }

    #[test]
    fn server_emits_cors_allow_all_header() {
        // P6.4: a browser page may fetch /metrics directly.
        // Without `access-control-allow-origin: *`, the browser rejects
        // the response and the live panel stays blank.
        let m = MissionMetrics::new();
        let server = MetricsServer::spawn("127.0.0.1:0".parse().unwrap(), m, labels()).unwrap();
        let mut stream =
            TcpStream::connect_timeout(&server.addr(), Duration::from_secs(2)).unwrap();
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nhost: localhost\r\n\r\n")
            .unwrap();
        let mut body = String::new();
        stream.read_to_string(&mut body).unwrap();
        assert!(
            body.to_ascii_lowercase()
                .contains("access-control-allow-origin: *"),
            "expected CORS header in response, got:\n{body}"
        );
    }

    #[test]
    fn server_shuts_down_cleanly_on_drop() {
        let m = MissionMetrics::new();
        let server = MetricsServer::spawn("127.0.0.1:0".parse().unwrap(), m, labels()).unwrap();
        let addr = server.addr();
        drop(server);
        // Give the OS a moment to release the port; some kernels take
        // a beat. The next connect should fail.
        thread::sleep(Duration::from_millis(100));
        let connect = TcpStream::connect_timeout(&addr, Duration::from_millis(200));
        assert!(
            connect.is_err(),
            "port must be released after server drop, but connect succeeded"
        );
    }
}
