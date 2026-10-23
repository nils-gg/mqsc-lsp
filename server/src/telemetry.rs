//! Optional file-based observability (T-14).
//!
//! Silent by default — a language server must not write anywhere unless
//! asked. Enable with `--log-file PATH` or `MQSC_LSP_LOG=path` (the flag
//! wins). In stdio mode stdout carries LSP traffic only; everything here
//! goes to the log file, with panics additionally echoed to stderr.
//!
//! What lands in the file:
//! - one line per JSON-RPC call (method, duration) and per slow notification
//! - `didOpen`/`didChange` with text length, parse time and define count
//! - `analyze` duration and diagnostic count per publish
//! - every panic: location from the panic hook plus the handler method
//!   caught by [`Observed`], which also turns the panic into a `-32603`
//!   response so one poisoned request cannot take the whole server down.

use std::fmt::Arguments;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tower::Service;
use tower_lsp::jsonrpc::{Error, Request, Response};

/// The log file, once enabled. `Mutex::new` in a static is const-stable;
/// File is Send, which is all Mutex needs to be Sync.
static LOG: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// Whether logging is active (used to skip building log strings entirely).
pub fn enabled() -> bool {
    LOG.lock().map(|g| g.is_some()).unwrap_or(false)
}

/// Open (or create, appending) `path` as the log and install the panic
/// hook. Called once from main when a target was configured.
pub fn init(path: &Path) -> std::io::Result<()> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    {
        let mut guard = LOG.lock().unwrap();
        *guard = Some(file);
    }
    install_panic_hook();
    log(format_args!(
        "mqsc-lsp {} logging to {}",
        env!("CARGO_PKG_VERSION"),
        path.display()
    ));
    Ok(())
}

/// Append one timestamped line. No-ops when logging is off.
pub fn log(msg: Arguments) {
    if let Ok(mut guard) = LOG.lock() {
        if let Some(file) = guard.as_mut() {
            let line = format!("{} {}\n", now_iso(), msg);
            // A failing write must never take the server down.
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
        }
    }
}

/// Convenience: build and write only when enabled.
#[macro_export]
macro_rules! tlog {
    ($($arg:tt)*) => {
        if $crate::telemetry::enabled() {
            $crate::telemetry::log(std::format_args!($($arg)*));
        }
    };
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "?".to_string());
        let payload = info.payload();
        let msg = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(|s| s.as_str()))
            .unwrap_or("panic with non-string payload");
        eprintln!("mqsc-lsp panic at {loc}: {msg}");
        log(format_args!("PANIC at {loc}: {msg}"));
    }));
}

/// UTC `YYYY-MM-DDThh:mm:ss.mmmZ` without pulling in a date crate
/// (days-to-civil algorithm by Howard Hinnant).
fn now_iso() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let (secs, millis) = (ms.div_euclid(1000), ms.rem_euclid(1000));
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

// ------------------------------------------------------- request observation

/// Tower service wrapper that instruments every JSON-RPC call reaching an
/// `LspService`: duration logging plus panic catching (see module docs).
/// Clone through: the wrapper is created once per connection.
#[derive(Clone)]
pub struct Observed<S> {
    inner: S,
}

impl<S> Observed<S> {
    pub fn new(inner: S) -> Self {
        Observed { inner }
    }
}

/// Notifications below this many milliseconds are not logged — didChange
/// fires per keystroke, and the interesting per-document lines (parse,
/// analyze) are written by the server itself anyway.
const NOTIF_LOG_MS: u128 = 200;

impl<S> Service<Request> for Observed<S>
where
    S: Service<Request, Response = Option<Response>>,
    S::Future: Future<Output = Result<Option<Response>, S::Error>> + Send + Unpin + 'static,
    S::Error: Send + 'static,
{
    type Response = Option<Response>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Option<Response>, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let method = req.method().to_string();
        let id = req.id().cloned();
        let t0 = Instant::now();
        let fut = CatchPanic {
            inner: self.inner.call(req),
            method: method.clone(),
        };
        Box::pin(async move {
            let out = fut.await;
            let ms = t0.elapsed().as_millis();
            match out {
                Ok(Ok(resp)) => {
                    if id.is_some() || ms >= NOTIF_LOG_MS {
                        log(format_args!(
                            "req {method} {}-> ok in {ms}ms",
                            if id.is_some() { "" } else { "(slow notif) " }
                        ));
                    }
                    Ok(resp)
                }
                Ok(Err(e)) => {
                    log(format_args!("req {method} -> service error in {ms}ms"));
                    Err(e)
                }
                Err(panic_msg) => {
                    // CatchPanic already logged the PANIC line; now make the
                    // connection survivable: answer the request, swallow the
                    // notification.
                    match id {
                        Some(id) => {
                            let err = Error {
                                data: Some(serde_json::Value::String(panic_msg)),
                                ..Error::internal_error()
                            };
                            Ok(Some(Response::from_parts(id, Err(err))))
                        }
                        None => Ok(None),
                    }
                }
            }
        })
    }
}

/// Catches a panic raised while polling `inner`. This is the only place
/// panic unwinding can be caught in async code: the panic happens during
/// `poll`, so `catch_unwind` wraps exactly that call (tower's own
/// `CatchPanic` layer works the same way).
struct CatchPanic<F> {
    inner: F,
    method: String,
}

impl<F: Future + Unpin> Future for CatchPanic<F> {
    type Output = Result<F::Output, String>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.get_mut();
        match std::panic::catch_unwind(AssertUnwindSafe(|| Pin::new(&mut me.inner).poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(v)) => Poll::Ready(Ok(v)),
            Err(payload) => {
                let msg = payload
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(|s| s.as_str()))
                    .unwrap_or("panic with non-string payload")
                    .to_string();
                log(format_args!("PANIC while polling `{}`: {msg}", me.method));
                Poll::Ready(Err(msg))
            }
        }
    }
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::jsonrpc::{ErrorCode, Id};

    /// init() flips a process-global; tests that init and read back a log
    /// file must not interleave.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn isolated_init(tag: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("mqsc-lsp-log-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        init(&path).expect("init log file");
        path
    }

    #[test]
    fn timestamp_format_is_utc_iso() {
        let t = now_iso();
        // 2024-02-29T13:45:59.123Z shape
        assert_eq!(t.len(), 24, "{t}");
        assert_eq!(&t[4..5], "-");
        assert_eq!(&t[10..11], "T");
        assert!(t.ends_with('Z'), "{t}");
    }

    #[test]
    fn log_file_receives_entries_when_enabled() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = isolated_init("write");
        crate::tlog!("hello {}", 42);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("hello 42"), "{text}");
        assert!(text.contains("logging to"), "{text}");
        let _ = std::fs::remove_file(&path);
    }

    /// A service whose request handlers panic mid-poll.
    struct Panicky;

    impl Service<Request> for Panicky {
        type Response = Option<Response>;
        type Error = std::io::Error;
        type Future =
            Pin<Box<dyn Future<Output = Result<Option<Response>, std::io::Error>> + Send>>;

        fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: Request) -> Self::Future {
            // panic on requests *and* notifications — Observed must answer
            // the first with -32603 and swallow the second
            Box::pin(async { panic!("boom in handler") })
        }
    }

    // The std-guard-across-await is deliberate: it serialises two tests that
    // share the process-global log target for their whole body, awaits included.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn panicking_handler_becomes_internal_error_not_a_dead_server() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = isolated_init("panic");
        let mut svc = Observed::new(Panicky);

        let req = Request::build("textDocument/hover")
            .id(Id::Number(7))
            .finish();
        let resp = svc
            .call(req)
            .await
            .expect("service survives the panic")
            .expect("requests get an answer");
        let (_id, body) = resp.into_parts();
        let err = body.expect_err("panicked request must be an error response");
        assert!(
            matches!(err.code, ErrorCode::InternalError),
            "{:?}",
            err.code
        );
        assert!(
            err.data
                .as_ref()
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .contains("boom in handler"),
            "{err:?}"
        );

        // a panicking notification is swallowed — nothing to answer
        let notif = Request::build("textDocument/didChange").finish();
        assert!(svc.call(notif).await.unwrap().is_none());

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("PANIC while polling `textDocument/hover`"),
            "{text}"
        );
        assert!(
            text.contains("PANIC while polling `textDocument/didChange`"),
            "{text}"
        );
        let _ = std::fs::remove_file(&path);
    }
}
