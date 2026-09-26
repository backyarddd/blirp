//! Rate limit for log lines that repeat at steady state.
//!
//! Some sources log the same line forever: iroh's mDNS (swarm-discovery)
//! logs "error sending mDNS: No route to host" several times a second when
//! macOS denies blirp the Local Network permission, and retry loops or the
//! status tick repeat one failure for as long as it lasts. For the targets
//! in [`LIMITED_TARGETS`], a line (target, level, message and fields) seen
//! within the last [`WINDOW`] is dropped and counted; the first one after
//! the window is logged with the count appended. Every other line, and each
//! distinct line of a limited target, is logged as before.
//!
//! [`LogLimit`] decides once per event (in `event_enabled`, so the line is
//! dropped for every output) and hands the count of a line that ends a
//! window to [`WithSuppressedCount`], the formatter, through a thread local:
//! both run synchronously on the thread that emits the event.

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// A repeated line is logged at most once per window.
pub const WINDOW: Duration = Duration::from_secs(10 * 60);

/// Targets (and their submodules) known to repeat lines at steady state.
pub const LIMITED_TARGETS: &[&str] = &[
    // mDNS sends, several a second while the LAN is unreachable.
    "swarm_discovery",
    "iroh_mdns_address_lookup",
    // Relay and path retries while offline.
    "iroh",
    // Node reconnect loop; the hub's 500 ms hub_log tick.
    "blirp_sync::service",
    // The 500 ms status tick and the status heuristics it runs.
    "blirp::daemon",
    "blirp::sessions",
    // Retried every minute while sessions run.
    "blirp::keep_awake",
    // Watcher errors, the 30 s sweep, per-pass status writes.
    "blirp::ingest",
    // The 30 s distill scheduler.
    "blirp::memory::distill",
];

/// Distinct lines tracked at once. When full, lines whose window passed are
/// forgotten; a new line that still finds no room is logged untracked.
const MAX_TRACKED: usize = 1024;

fn is_limited(target: &str) -> bool {
    LIMITED_TARGETS.iter().any(|t| {
        target
            .strip_prefix(t)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
    })
}

/// What to do with one occurrence of a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Log it; `suppressed` identical lines were dropped since it was last logged.
    Log {
        suppressed: u64,
    },
    Drop,
}

struct Seen {
    logged_at: Instant,
    suppressed: u64,
}

/// The bookkeeping, with the clock passed in.
#[derive(Default)]
pub struct Limiter {
    seen: HashMap<(&'static str, Level, String), Seen>,
}

impl Limiter {
    pub fn check(
        &mut self,
        target: &'static str,
        level: Level,
        text: String,
        now: Instant,
    ) -> Verdict {
        let key = (target, level, text);
        if let Some(s) = self.seen.get_mut(&key) {
            if now.saturating_duration_since(s.logged_at) < WINDOW {
                s.suppressed += 1;
                return Verdict::Drop;
            }
            let suppressed = std::mem::take(&mut s.suppressed);
            s.logged_at = now;
            return Verdict::Log { suppressed };
        }
        if self.seen.len() >= MAX_TRACKED {
            self.seen
                .retain(|_, s| now.saturating_duration_since(s.logged_at) < WINDOW);
        }
        if self.seen.len() < MAX_TRACKED {
            self.seen.insert(
                key,
                Seen {
                    logged_at: now,
                    suppressed: 0,
                },
            );
        }
        Verdict::Log { suppressed: 0 }
    }
}

/// Message and fields of an event, as the key of its line.
struct LineText(String);

impl Visit for LineText {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // Writing to a String cannot fail.
        let _ = write!(self.0, "{}={value:?};", field.name());
    }
}

thread_local! {
    /// The event being dispatched on this thread (its address) and how many
    /// identical lines were dropped before it; 0 for everything else.
    static PENDING: Cell<(usize, u64)> = const { Cell::new((0, 0)) };
}

fn event_addr(event: &Event<'_>) -> usize {
    std::ptr::from_ref(event) as usize
}

/// The layer that drops repeats. Add it next to the formatting layers, and
/// wrap their event format in [`WithSuppressedCount`].
pub struct LogLimit {
    limiter: Arc<Mutex<Limiter>>,
    clock: Box<dyn Fn() -> Instant + Send + Sync>,
}

impl Default for LogLimit {
    fn default() -> Self {
        Self::with_clock(Instant::now)
    }
}

impl LogLimit {
    pub fn with_clock(clock: impl Fn() -> Instant + Send + Sync + 'static) -> Self {
        Self {
            limiter: Arc::default(),
            clock: Box::new(clock),
        }
    }
}

impl<S: Subscriber> Layer<S> for LogLimit {
    fn event_enabled(&self, event: &Event<'_>, _ctx: Context<'_, S>) -> bool {
        let meta = event.metadata();
        if !is_limited(meta.target()) {
            PENDING.set((0, 0));
            return true;
        }
        let mut text = LineText(String::new());
        event.record(&mut text);
        let verdict = self
            .limiter
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .check(meta.target(), *meta.level(), text.0, (self.clock)());
        match verdict {
            Verdict::Log { suppressed } => {
                PENDING.set((event_addr(event), suppressed));
                true
            }
            Verdict::Drop => false,
        }
    }
}

/// Event format that appends the count of dropped repeats to the line that
/// ends their window.
pub struct WithSuppressedCount<F>(pub F);

impl<S, N, F> FormatEvent<S, N> for WithSuppressedCount<F>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    F: FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let suppressed = match PENDING.get() {
            (addr, n) if addr == event_addr(event) => n,
            _ => 0,
        };
        if suppressed == 0 {
            return self.0.format_event(ctx, writer, event);
        }
        // Formatted without color: the count goes before the newline.
        let mut line = String::new();
        self.0.format_event(ctx, Writer::new(&mut line), event)?;
        let line = line.strip_suffix('\n').unwrap_or(&line);
        let minutes = WINDOW.as_secs() / 60;
        writeln!(
            writer,
            "{line} ({suppressed} identical lines suppressed in the last {minutes} min)"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::fmt::MakeWriter;
    use tracing_subscriber::layer::SubscriberExt as _;

    fn check(l: &mut Limiter, text: &str, now: Instant) -> Verdict {
        l.check("swarm_discovery::socket", Level::WARN, text.into(), now)
    }

    #[test]
    fn logs_first_drops_repeats_and_counts_them_after_the_window() {
        let t0 = Instant::now();
        let mut l = Limiter::default();
        let msg = "error sending mDNS: No route to host";
        assert_eq!(check(&mut l, msg, t0), Verdict::Log { suppressed: 0 });
        for i in 1..=5 {
            assert_eq!(
                check(&mut l, msg, t0 + Duration::from_secs(i)),
                Verdict::Drop
            );
        }
        // Another line of the same target is not held back.
        assert_eq!(
            check(
                &mut l,
                "error sending mDNS on IPv6: x",
                t0 + Duration::from_secs(2)
            ),
            Verdict::Log { suppressed: 0 }
        );
        assert_eq!(
            check(&mut l, msg, t0 + WINDOW - Duration::from_millis(1)),
            Verdict::Drop
        );
        assert_eq!(
            check(&mut l, msg, t0 + WINDOW),
            Verdict::Log { suppressed: 6 }
        );
        // A new window starts there, with a fresh count.
        assert_eq!(
            check(&mut l, msg, t0 + WINDOW + Duration::from_secs(1)),
            Verdict::Drop
        );
        assert_eq!(
            check(&mut l, msg, t0 + WINDOW * 2),
            Verdict::Log { suppressed: 1 }
        );
        // Quiet for a whole window: logged again, nothing to report.
        assert_eq!(
            check(&mut l, msg, t0 + WINDOW * 4),
            Verdict::Log { suppressed: 0 }
        );
    }

    #[test]
    fn level_is_part_of_the_line() {
        let t0 = Instant::now();
        let mut l = Limiter::default();
        assert_eq!(
            l.check("iroh", Level::WARN, "x".into(), t0),
            Verdict::Log { suppressed: 0 }
        );
        assert_eq!(
            l.check("iroh", Level::ERROR, "x".into(), t0),
            Verdict::Log { suppressed: 0 }
        );
        assert_eq!(l.check("iroh", Level::WARN, "x".into(), t0), Verdict::Drop);
    }

    #[test]
    fn full_table_forgets_old_lines_and_never_drops_untracked_ones() {
        let t0 = Instant::now();
        let mut l = Limiter::default();
        for i in 0..MAX_TRACKED {
            check(&mut l, &i.to_string(), t0);
        }
        // Full and nothing expired: logged, and logged again (untracked).
        assert_eq!(check(&mut l, "new", t0), Verdict::Log { suppressed: 0 });
        assert_eq!(check(&mut l, "new", t0), Verdict::Log { suppressed: 0 });
        // Once the window passed, old lines make room.
        assert_eq!(
            check(&mut l, "new", t0 + WINDOW),
            Verdict::Log { suppressed: 0 }
        );
        assert_eq!(check(&mut l, "new", t0 + WINDOW), Verdict::Drop);
    }

    #[test]
    fn only_listed_targets_and_their_modules_are_limited() {
        assert!(is_limited("swarm_discovery"));
        assert!(is_limited("swarm_discovery::socket"));
        assert!(is_limited("iroh::socket::transports::relay::actor"));
        assert!(is_limited("blirp::ingest::service"));
        assert!(!is_limited("iroh_relay"));
        assert!(!is_limited("blirp::api"));
        assert!(!is_limited("blirp::ingestx"));
        assert!(!is_limited("blirp"));
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Capture {
        type Writer = Capture;
        fn make_writer(&'a self) -> Capture {
            self.clone()
        }
    }

    #[test]
    fn subscriber_drops_repeats_and_appends_the_count() {
        let t0 = Instant::now();
        let now = Arc::new(Mutex::new(t0));
        let clock = {
            let now = now.clone();
            move || *now.lock().unwrap()
        };
        let out = Capture::default();
        let subscriber = tracing_subscriber::registry()
            .with(LogLimit::with_clock(clock))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(out.clone())
                    .with_ansi(false)
                    .event_format(WithSuppressedCount(
                        tracing_subscriber::fmt::format().without_time(),
                    )),
            );
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..4 {
                tracing::warn!(target: "swarm_discovery::socket", "error sending mDNS: {}", "No route to host");
            }
            tracing::warn!(target: "blirp::api", "not limited");
            tracing::warn!(target: "blirp::api", "not limited");
            *now.lock().unwrap() = t0 + WINDOW;
            tracing::warn!(target: "swarm_discovery::socket", "error sending mDNS: {}", "No route to host");
        });
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            [
                " WARN swarm_discovery::socket: error sending mDNS: No route to host",
                " WARN blirp::api: not limited",
                " WARN blirp::api: not limited",
                " WARN swarm_discovery::socket: error sending mDNS: No route to host (3 identical lines suppressed in the last 10 min)",
            ],
            "{text}"
        );
    }
}
