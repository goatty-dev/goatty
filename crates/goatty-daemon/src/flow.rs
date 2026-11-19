//! Per-connection adaptive flow-control window (#9).
//!
//! Remote links start at `StreamOpts::REMOTE` (256 KiB) and adapt across
//! `StreamOpts::MIN_WINDOW..=StreamOpts::MAX_WINDOW` (64 KiB..=8 MiB):
//!
//! * Delivery rate is measured on the sender (daemon) from `Ack` progress while
//!   bytes are actually in flight.
//! * Propagation delay (`min_rtt`) comes from the client's `Ping`/`Pong`
//!   measurements (`ClientMsg::Rtt`) using a rolling minimum so self-inflicted
//!   queuing delay (bufferbloat) never inflates the window.
//! * Target window: `clamp(rate * (50 ms + min(min_rtt, 200 ms)), 64 KiB, 8 MiB)`.
//!   - Fast LAN / loopback proxy ramps up to several MiB within tens of
//!     milliseconds and streams losslessly.
//!   - High-RTT fast WAN links grow past 1 BDP instead of stalling on a fixed
//!     256 KiB cap.
//!   - Slow links shrink down toward 64 KiB so skip-and-resync triggers early
//!     and Ctrl+C drains within ~50 ms + RTT.
//! * App-limited (idle/interactive) periods never lower the rate estimate: only
//!   samples taken under backpressure (`lagged` or `peak_in_flight >= window/2`)
//!   can reduce the window.
//! * Local Unix sockets (`StreamOpts::LOCAL`, 8 MiB) do not attach a
//!   `FlowControl` instance at all, keeping zero overhead on the local hot path.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use goatty_proto::StreamOpts;
use parking_lot::Mutex;

/// Queuing/burst delay target above propagation RTT (50 ms, #9).
const TARGET_DELAY: Duration = Duration::from_millis(50);
/// Upper bound on propagation RTT added to the delay horizon so extreme
/// latency links do not buffer multi-second floods.
const MAX_RTT_CONTRIB: Duration = Duration::from_millis(200);
/// Rolling window for the propagation `min_rtt` filter.
const MIN_RTT_WINDOW: Duration = Duration::from_secs(60);
/// Cap on stored RTT values from the peer.
const MAX_STORED_RTT_US: u64 = 10_000_000; // 10 s
/// Periodic rate-sampling interval during continuous streaming (matches
/// `TARGET_DELAY` so the window only grows on sustained throughput, never on
/// brief token-bucket or kernel-buffer bursts).
const SAMPLE_INTERVAL: Duration = Duration::from_millis(50);
/// Minimum delivered bytes required to form a rate sample (filters out small
/// interactive echoes and prompt updates).
const MIN_SAMPLE_BYTES: u64 = 32 << 10;
/// Floor on `Starting` net transmission time to prevent division-by-zero spikes.
const MIN_RATE_TIME: Duration = Duration::from_micros(200);
/// Floor on `Streaming` interval to avoid Ack-bunching spikes on short tails.
const MIN_STREAMING_TIME: Duration = Duration::from_millis(1);
/// Sanity cap on rate estimates (100 GB/s) to keep f64 -> u64 casts safe.
const MAX_RATE_BPS: f64 = 1e11;

/// Shared per-connection adaptive flow-control state.
///
/// The hot path (`window()`) is a single relaxed atomic load. Updates acquire a
/// leaf mutex that never calls back into `Session` locks or performs I/O.
pub(crate) struct FlowControl {
    window: AtomicU64,
    state: Mutex<Estimator>,
}

#[derive(Debug)]
struct Estimator {
    configured: bool,
    window: u64,
    min_window: u64,
    max_window: u64,
    notify_client: bool,
    last_notified: u64,
    rate_bps: Option<f64>,
    /// Primary rolling minimum RTT and when it was observed.
    min_rtt: Option<(Duration, Instant)>,
    /// Candidate minimum RTT observed in the second half of `MIN_RTT_WINDOW`,
    /// promoted when `min_rtt` expires so route changes adapt smoothly.
    next_min_rtt: Option<(Duration, Instant)>,
    /// Aggregate unacknowledged stream bytes across adaptive subscriptions on
    /// this connection.
    in_flight: u64,
    phase: Phase,
    /// Wall-clock delivery accumulator spanning brief `in_flight == 0` drains
    /// so fast LAN/loopback links that drain faster than `SAMPLE_INTERVAL`
    /// still measure sustained throughput and ramp up cleanly.
    up_epoch: Option<UpEpoch>,
}

#[derive(Debug, Clone, Copy)]
struct UpEpoch {
    started_at: Instant,
    last_activity: Instant,
    /// Uncompressed stream bytes acknowledged in this wall-clock epoch.
    /// Compressed snapshot bytes are excluded so zstd ratios never inflate
    /// live-stream bandwidth estimates across panes or epochs.
    uncompressed: u64,
}

#[derive(Debug, Clone, Copy)]
enum Phase {
    Idle,
    /// First sample after transitioning out of `Idle`: elapsed time includes
    /// one propagation RTT, so `min_rtt` is subtracted.
    Starting {
        started_at: Instant,
        uncompressed: u64,
        total: u64,
        peak_in_flight: u64,
        lagged: bool,
    },
    /// Continuous busy stream (`in_flight > 0` across epochs): both endpoints
    /// are `Ack` arrival times, so propagation RTT cancels out.
    Streaming {
        epoch_at: Instant,
        uncompressed: u64,
        total: u64,
        peak_in_flight: u64,
        lagged: bool,
    },
}

#[derive(Debug, Clone, Copy)]
struct EpochSample {
    uncompressed: u64,
    total: u64,
    elapsed: Duration,
    allow_up: bool,
    allow_down: bool,
    lagged: bool,
}

impl FlowControl {
    pub(crate) fn new() -> Self {
        let initial = u64::from(StreamOpts::REMOTE.window);
        Self {
            window: AtomicU64::new(initial),
            state: Mutex::new(Estimator {
                configured: false,
                window: initial,
                min_window: u64::from(StreamOpts::MIN_WINDOW),
                max_window: u64::from(StreamOpts::MAX_WINDOW),
                notify_client: false,
                last_notified: initial,
                rate_bps: None,
                min_rtt: None,
                next_min_rtt: None,
                in_flight: 0,
                phase: Phase::Idle,
                up_epoch: None,
            }),
        }
    }

    /// Initialize bounds on the connection's first adaptive subscription.
    /// Subsequent subscriptions on the same connection inherit the already
    /// adapted window and link measurements.
    pub(crate) fn init_if_needed(&self, initial_window: u64, min_window: u64, notify_client: bool) {
        let mut st = self.state.lock();
        if st.configured {
            return;
        }
        let max_w = u64::from(StreamOpts::MAX_WINDOW);
        let min_w = min_window.clamp(u64::from(StreamOpts::MIN_WINDOW), max_w);
        let init = initial_window.clamp(min_w, max_w);
        st.configured = true;
        st.window = init;
        st.min_window = min_w;
        st.max_window = max_w;
        st.notify_client = notify_client;
        st.last_notified = init;
        self.window.store(init, Ordering::Relaxed);
    }

    /// Active flow-control window in bytes (lock-free).
    pub(crate) fn window(&self) -> u64 {
        self.window.load(Ordering::Relaxed)
    }

    /// Record `stream_len` newly queued stream bytes.
    pub(crate) fn on_send(&self, stream_len: u64) {
        if stream_len == 0 {
            return;
        }
        let mut st = self.state.lock();
        let need_now = matches!(st.phase, Phase::Idle) || st.up_epoch.is_none();
        let now = if need_now { Some(Instant::now()) } else { None };
        st.on_send_inner(stream_len, now);
    }

    #[cfg(test)]
    fn on_send_at(&self, stream_len: u64, now: Instant) {
        if stream_len == 0 {
            return;
        }
        self.state.lock().on_send_inner(stream_len, Some(now));
    }

    /// Release unacknowledged bytes owned by a subscription that exited,
    /// detached, or dropped so aggregate `in_flight` cannot leak.
    pub(crate) fn on_drop_in_flight(&self, remaining: u64) {
        if remaining == 0 {
            return;
        }
        let mut st = self.state.lock();
        st.in_flight = st.in_flight.saturating_sub(remaining);
        if st.in_flight == 0 {
            st.phase = Phase::Idle;
            st.up_epoch = None;
        }
    }

    /// Record a client-reported `Ping`/`Pong` RTT in microseconds. Returns a
    /// new window to send via `ServerMsg::FlowWindow` if it changed materially.
    pub(crate) fn on_rtt(&self, rtt_us: u64) -> Option<u32> {
        self.on_rtt_at(rtt_us, Instant::now())
    }

    pub(crate) fn on_rtt_at(&self, rtt_us: u64, now: Instant) -> Option<u32> {
        if rtt_us == 0 {
            return None;
        }
        let rtt = Duration::from_micros(rtt_us.min(MAX_STORED_RTT_US));
        let mut st = self.state.lock();
        st.record_rtt(rtt, now);
        if !st.configured {
            return None;
        }
        if let Some(rate) = st.rate_bps {
            let prev = st.window;
            let raw_target = st.target_window(rate, now);
            let next = if raw_target > prev { raw_target.min(prev.saturating_mul(2)) } else { raw_target };
            st.window = next.clamp(st.min_window, st.max_window);
            self.window.store(st.window, Ordering::Relaxed);
            return st.take_notification();
        }
        None
    }

    /// Record `delta` newly acknowledged stream bytes, of which
    /// `compressed_delta` came from zstd-compressed snapshots (charged to
    /// `in_flight` but excluded from upward live-rate samples), with whether
    /// the subscriber was `sub_lagged`. Returns a new window to send via
    /// `ServerMsg::FlowWindow` if it changed materially.
    pub(crate) fn on_ack(&self, delta: u64, compressed_delta: u64, sub_lagged: bool) -> Option<u32> {
        self.on_ack_at(delta, compressed_delta, sub_lagged, Instant::now())
    }

    pub(crate) fn on_ack_at(&self, delta: u64, compressed_delta: u64, sub_lagged: bool, now: Instant) -> Option<u32> {
        if delta == 0 {
            return None;
        }
        let mut st = self.state.lock();
        let notify = st.on_ack_inner(delta, compressed_delta.min(delta), sub_lagged, now);
        self.window.store(st.window, Ordering::Relaxed);
        notify
    }

    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> u64 {
        self.state.lock().in_flight
    }
}

impl Estimator {
    fn on_send_inner(&mut self, stream_len: u64, now: Option<Instant>) {
        self.in_flight = self.in_flight.saturating_add(stream_len);
        match &mut self.up_epoch {
            None => {
                let t = now.unwrap_or_else(Instant::now);
                self.up_epoch = Some(UpEpoch { started_at: t, last_activity: t, uncompressed: 0 });
            }
            Some(ep) => {
                if let Some(t) = now {
                    if t.saturating_duration_since(ep.last_activity) > SAMPLE_INTERVAL {
                        *ep = UpEpoch { started_at: t, last_activity: t, uncompressed: 0 };
                    } else {
                        ep.last_activity = t;
                    }
                }
            }
        }
        match &mut self.phase {
            Phase::Idle => {
                self.phase = Phase::Starting {
                    started_at: now.unwrap_or_else(Instant::now),
                    uncompressed: 0,
                    total: 0,
                    peak_in_flight: self.in_flight,
                    lagged: false,
                };
            }
            Phase::Starting { peak_in_flight, .. } | Phase::Streaming { peak_in_flight, .. } => {
                *peak_in_flight = (*peak_in_flight).max(self.in_flight);
            }
        }
    }

    fn advance_up_epoch(&mut self, uncompressed_delta: u64, drained: bool, reset_only: bool, now: Instant) {
        if reset_only {
            self.up_epoch = if drained { None } else { Some(UpEpoch { started_at: now, last_activity: now, uncompressed: 0 }) };
            return;
        }
        let Some(mut ep) = self.up_epoch else {
            return;
        };
        ep.uncompressed = ep.uncompressed.saturating_add(uncompressed_delta);
        ep.last_activity = now;
        let dt = now.saturating_duration_since(ep.started_at);
        if dt >= SAMPLE_INTERVAL {
            self.up_epoch = if drained { None } else { Some(UpEpoch { started_at: now, last_activity: now, uncompressed: 0 }) };
            if ep.uncompressed >= MIN_SAMPLE_BYTES {
                let wall_bps = ep.uncompressed as f64 / dt.as_secs_f64();
                self.apply_sample(wall_bps, true, false, false, now);
            }
        } else {
            self.up_epoch = Some(ep);
        }
    }

    fn apply_epoch(&mut self, s: EpochSample, now: Instant) {
        let secs = s.elapsed.as_secs_f64();
        if s.uncompressed == s.total {
            if s.total >= MIN_SAMPLE_BYTES {
                self.apply_sample(s.total as f64 / secs, s.allow_up, s.allow_down, s.lagged, now);
            }
            return;
        }
        // Mixed uncompressed + zstd-compressed bytes in one epoch:
        //   uncompressed / dt <= true wire rate <= total / dt
        // Upward growth uses the lower bound (`uncompressed`), downward
        // shrinking uses the upper bound (`total`), so compressed snapshots
        // can neither inflate the window nor cause false slow-link shrinks.
        if s.allow_up && s.uncompressed >= MIN_SAMPLE_BYTES {
            self.apply_sample(s.uncompressed as f64 / secs, true, false, false, now);
        }
        if s.allow_down && s.total >= MIN_SAMPLE_BYTES {
            self.apply_sample(s.total as f64 / secs, false, true, s.lagged, now);
        }
    }

    fn on_ack_inner(&mut self, delta: u64, compressed_delta: u64, sub_lagged: bool, now: Instant) -> Option<u32> {
        let uncompressed_delta = delta.saturating_sub(compressed_delta);
        self.in_flight = self.in_flight.saturating_sub(delta);
        let drained = self.in_flight == 0;
        match self.phase {
            Phase::Idle => None,
            Phase::Starting { started_at, mut uncompressed, mut total, peak_in_flight, mut lagged } => {
                uncompressed = uncompressed.saturating_add(uncompressed_delta);
                total = total.saturating_add(delta);
                lagged |= sub_lagged;
                let raw = now.saturating_duration_since(started_at);
                let sustained = raw >= SAMPLE_INTERVAL;
                if !drained && !sustained {
                    self.phase = Phase::Starting { started_at, uncompressed, total, peak_in_flight, lagged };
                    self.advance_up_epoch(uncompressed_delta, false, false, now);
                    return self.take_notification();
                }
                self.phase = if drained {
                    Phase::Idle
                } else {
                    Phase::Streaming { epoch_at: now, uncompressed: 0, total: 0, peak_in_flight: self.in_flight, lagged: sub_lagged }
                };
                if total >= MIN_SAMPLE_BYTES {
                    let min_rtt = self.effective_min_rtt(now);
                    if let Some(rtt) = min_rtt
                        && raw < rtt
                    {
                        // An Ack cannot return faster than true propagation RTT,
                        // so `raw` is a tighter upper bound than a stale/jittery
                        // Ping measurement. Tighten `min_rtt` and skip this sample.
                        self.record_rtt(raw.max(Duration::from_micros(1)), now);
                    } else {
                        let net = raw.saturating_sub(min_rtt.unwrap_or(Duration::ZERO));
                        let backpressured = lagged || peak_in_flight >= self.window / 2;
                        if net < MIN_RATE_TIME {
                            if sustained && !lagged && uncompressed >= MIN_SAMPLE_BYTES {
                                let sample_bps = uncompressed as f64 / MIN_RATE_TIME.as_secs_f64();
                                self.apply_sample(sample_bps, true, false, false, now);
                            }
                        } else {
                            self.apply_epoch(
                                EpochSample { uncompressed, total, elapsed: net, allow_up: sustained, allow_down: backpressured, lagged },
                                now,
                            );
                        }
                    }
                }
                self.advance_up_epoch(uncompressed_delta, drained, sustained, now);
                self.take_notification()
            }
            Phase::Streaming { epoch_at, mut uncompressed, mut total, peak_in_flight, mut lagged } => {
                uncompressed = uncompressed.saturating_add(uncompressed_delta);
                total = total.saturating_add(delta);
                lagged |= sub_lagged;
                let dt = now.saturating_duration_since(epoch_at);
                let sustained = dt >= SAMPLE_INTERVAL;
                if !drained && !sustained {
                    self.phase = Phase::Streaming { epoch_at, uncompressed, total, peak_in_flight, lagged };
                    self.advance_up_epoch(uncompressed_delta, false, false, now);
                    return self.take_notification();
                }
                self.phase = if drained {
                    Phase::Idle
                } else {
                    Phase::Streaming { epoch_at: now, uncompressed: 0, total: 0, peak_in_flight: self.in_flight, lagged: sub_lagged }
                };
                if total >= MIN_SAMPLE_BYTES && dt >= MIN_STREAMING_TIME {
                    let backpressured = lagged || peak_in_flight >= self.window / 2;
                    self.apply_epoch(
                        EpochSample { uncompressed, total, elapsed: dt, allow_up: sustained, allow_down: backpressured, lagged },
                        now,
                    );
                }
                self.advance_up_epoch(uncompressed_delta, drained, sustained, now);
                self.take_notification()
            }
        }
    }

    fn expire_min_rtt(&mut self, now: Instant) {
        if let Some((_, at)) = self.min_rtt
            && now.saturating_duration_since(at) >= MIN_RTT_WINDOW
        {
            self.min_rtt = self.next_min_rtt.take().filter(|(_, next_at)| now.saturating_duration_since(*next_at) < MIN_RTT_WINDOW);
        }
    }

    fn record_rtt(&mut self, rtt: Duration, now: Instant) {
        self.expire_min_rtt(now);
        if self.min_rtt.is_none_or(|(cur, _)| rtt <= cur) {
            self.min_rtt = Some((rtt, now));
        }
        if let Some((_, at)) = self.min_rtt
            && now.saturating_duration_since(at) >= MIN_RTT_WINDOW / 2
            && self.next_min_rtt.is_none_or(|(cur, _)| rtt <= cur)
        {
            self.next_min_rtt = Some((rtt, now));
        }
    }

    fn effective_min_rtt(&mut self, now: Instant) -> Option<Duration> {
        self.expire_min_rtt(now);
        self.min_rtt.map(|(r, _)| r)
    }

    fn horizon(&mut self, now: Instant) -> Duration {
        let rtt = self.effective_min_rtt(now).unwrap_or(Duration::ZERO).min(MAX_RTT_CONTRIB);
        TARGET_DELAY + rtt
    }

    fn target_window(&mut self, rate_bps: f64, now: Instant) -> u64 {
        let bytes = (rate_bps.clamp(0.0, MAX_RATE_BPS) * self.horizon(now).as_secs_f64()).round() as u64;
        bytes.clamp(self.min_window, self.max_window)
    }

    fn apply_sample(&mut self, sample_bps: f64, allow_up: bool, allow_down: bool, lagged: bool, now: Instant) {
        if !self.configured || !sample_bps.is_finite() || sample_bps <= 0.0 {
            return;
        }
        let sample_bps = sample_bps.min(MAX_RATE_BPS);
        let prev_window = self.window;
        let sample_target = self.target_window(sample_bps, now);
        let mut growing = false;
        let next_rate = match self.rate_bps {
            None => {
                if sample_target > prev_window {
                    if !allow_up {
                        return;
                    }
                    growing = true;
                    sample_bps
                } else if sample_target < prev_window {
                    if !allow_down {
                        return;
                    }
                    sample_bps
                } else {
                    sample_bps
                }
            }
            Some(prev) => {
                if sample_bps > prev {
                    if !allow_up {
                        return;
                    }
                    growing = true;
                    prev * 0.25 + sample_bps * 0.75
                } else if sample_bps < prev {
                    if !allow_down {
                        return;
                    }
                    // Converge quickly downward when actively lagging so slow
                    // links shrink on the first flood cycle.
                    let alpha = if lagged { 0.75 } else { 0.5 };
                    prev * (1.0 - alpha) + sample_bps * alpha
                } else {
                    growing = sample_target > prev_window && allow_up;
                    prev
                }
            }
        };

        let raw_target = self.target_window(next_rate, now);
        let new_window = if raw_target > prev_window {
            if !growing {
                prev_window
            } else {
                // Slow-start doubling per sustained 50 ms epoch: reaches 8 MiB
                // in 250 ms on a fast LAN without overshooting on brief bursts.
                raw_target.min(prev_window.saturating_mul(2))
            }
        } else {
            raw_target
        }
        .clamp(self.min_window, self.max_window);

        // Keep `rate_bps` consistent when upward growth was step-capped so a
        // brief burst cannot leave an inflated rate behind if the link stalls.
        let horizon_s = self.horizon(now).as_secs_f64();
        let stored_rate = if new_window < raw_target && horizon_s > 0.0 { next_rate.min(new_window as f64 / horizon_s) } else { next_rate };
        self.rate_bps = Some(stored_rate);
        self.window = new_window;
    }

    fn take_notification(&mut self) -> Option<u32> {
        if !self.configured || !self.notify_client {
            return None;
        }
        if !should_notify(self.last_notified, self.window, self.min_window, self.max_window) {
            return None;
        }
        self.last_notified = self.window;
        Some(u32::try_from(self.window).unwrap_or(StreamOpts::MAX_WINDOW))
    }
}

fn should_notify(last: u64, next: u64, min_w: u64, max_w: u64) -> bool {
    if last == next {
        return false;
    }
    if next == min_w || next == max_w {
        return true;
    }
    next.abs_diff(last).saturating_mul(4) >= last
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;

    fn configured(initial: u64, min_w: u64) -> FlowControl {
        let fc = FlowControl::new();
        fc.init_if_needed(initial, min_w, true);
        fc
    }

    #[test]
    fn fast_lan_ramps_up_to_max_window() {
        let fc = configured(256 * KIB, 64 * KIB);
        let t0 = Instant::now();
        fc.on_rtt_at(500, t0); // 0.5 ms LAN RTT

        // Simulate continuous 250 MB/s delivery (64 KiB every 256 us for ~307 ms,
        // spanning 6 sustained 50 ms epochs: 256 KiB -> 512 -> 1M -> 2M -> 4M -> 8M).
        let mut t = t0;
        let mut notified = Vec::new();
        for _ in 0..1200 {
            fc.on_send_at(64 * KIB, t);
            t += Duration::from_micros(256);
            if let Some(w) = fc.on_ack_at(64 * KIB, 0, false, t) {
                notified.push(w);
            }
        }
        assert_eq!(fc.window(), 8 * MIB, "fast 250 MB/s link should reach 8 MiB max window");
        assert_eq!(notified.last().copied(), Some(StreamOpts::MAX_WINDOW));
    }

    #[test]
    fn slow_link_shrinks_toward_floor_and_skips_early() {
        let fc = configured(256 * KIB, 64 * KIB);
        let t0 = Instant::now();
        fc.on_rtt_at(1_000, t0); // 1 ms RTT

        // Daemon fills 256 KiB window and goes lagging; link delivers at 500 KB/s.
        fc.on_send_at(256 * KIB, t0);
        // 64 KiB takes ~131 ms at 500 KB/s (plus 1 ms RTT).
        let mut t = t0 + Duration::from_millis(1);
        for _ in 0..4 {
            t += Duration::from_millis(131);
            fc.on_ack_at(64 * KIB, 0, true, t);
        }
        assert_eq!(fc.window(), 64 * KIB, "500 KB/s link should shrink to 64 KiB floor");
    }

    #[test]
    fn moderate_slow_link_targets_rate_times_50ms() {
        let fc = configured(256 * KIB, 64 * KIB);
        let t0 = Instant::now();
        fc.on_rtt_at(1_000, t0); // 1 ms RTT

        // 2 MB/s link: target ~ 2 MB/s * 51 ms ~= 102 KiB.
        fc.on_send_at(256 * KIB, t0);
        let mut t = t0 + Duration::from_millis(1);
        for _ in 0..4 {
            t += Duration::from_millis(32); // 64 KiB / 32 ms = 2 MiB/s
            fc.on_ack_at(64 * KIB, 0, true, t);
        }
        let w = fc.window();
        assert!((85 * KIB..=130 * KIB).contains(&w), "expected ~102 KiB for 2 MB/s link, got {} KiB", w / KIB);
    }

    #[test]
    fn high_rtt_fast_wan_exceeds_bdp() {
        let fc = configured(256 * KIB, 64 * KIB);
        let t0 = Instant::now();
        fc.on_rtt_at(80_000, t0); // 80 ms RTT

        // 100 Mbps (12.5 MB/s) link, BDP = 1.0 MiB, target = 12.5 MB/s * 130 ms ~= 1.6 MiB.
        let mut t = t0;
        for _ in 0..4 {
            let w = fc.window();
            fc.on_send_at(w, t);
            // Flight finishes after RTT + w / 12.5 MB/s.
            let tx_us = (w as f64 / 12.5e6 * 1e6) as u64;
            t += Duration::from_millis(80) + Duration::from_micros(tx_us);
            fc.on_ack_at(w, 0, true, t);
        }
        let w = fc.window();
        assert!((1200 * KIB..=2200 * KIB).contains(&w), "expected ~1.6 MiB (> 1 MiB BDP), got {} KiB", w / KIB);
    }

    #[test]
    fn app_limited_interactive_traffic_never_shrinks_window() {
        let fc = configured(256 * KIB, 64 * KIB);
        let t0 = Instant::now();
        fc.on_rtt_at(1_000, t0);

        // Ramp up on a fast burst first (~300 ms).
        let mut t = t0;
        for _ in 0..1000 {
            fc.on_send_at(64 * KIB, t);
            t += Duration::from_micros(300);
            fc.on_ack_at(64 * KIB, 0, false, t);
        }
        let fast_w = fc.window();
        assert!(fast_w >= 4 * MIB, "expected ramped window, got {} KiB", fast_w / KIB);

        // Now simulate slow producer: 10 KiB every 50 ms (200 KB/s) with low in_flight.
        for _ in 0..40 {
            t += Duration::from_millis(50);
            fc.on_send_at(10 * KIB, t);
            t += Duration::from_millis(2);
            fc.on_ack_at(10 * KIB, 0, false, t);
        }
        assert_eq!(fc.window(), fast_w, "app-limited traffic must not shrink window");
    }

    #[test]
    fn bufferbloated_rtt_spikes_do_not_inflate_window() {
        let fc = configured(256 * KIB, 64 * KIB);
        let t0 = Instant::now();
        fc.on_rtt_at(20_000, t0); // 20 ms true propagation RTT

        // Measure a 2 MB/s link.
        fc.on_send_at(256 * KIB, t0);
        let t1 = t0 + Duration::from_millis(20 + 128);
        fc.on_ack_at(256 * KIB, 0, true, t1);
        let baseline = fc.window();

        // Bufferbloated 500 ms Pong during congestion must be ignored by min_rtt.
        fc.on_rtt_at(500_000, t1 + Duration::from_secs(5));
        assert_eq!(fc.window(), baseline, "bufferbloat RTT spike must not grow window");
    }

    #[test]
    fn compressed_snapshot_does_not_inflate_window() {
        let fc = configured(256 * KIB, 64 * KIB);
        let t0 = Instant::now();
        fc.on_rtt_at(1_000, t0);

        // 100 KiB uncompressed snapshot that took only 2 ms because zstd shrank it.
        fc.on_send_at(100 * KIB, t0);
        fc.on_ack_at(100 * KIB, 100 * KIB, false, t0 + Duration::from_millis(3));
        assert_eq!(fc.window(), 256 * KIB, "compressed snapshot must not inflate live-stream window");
    }

    #[test]
    fn cross_pane_ack_before_compressed_snapshot_does_not_inflate_window() {
        let fc = configured(256 * KIB, 64 * KIB);
        let t0 = Instant::now();
        fc.on_rtt_at(1_000, t0);

        // Pane B queues a 2 MiB compressed snapshot; Pane A queues 64 KiB of raw output.
        fc.on_send_at(2 * MIB, t0);
        fc.on_send_at(64 * KIB, t0);

        // Pane A's Ack arrives at t=55 ms, advancing Phase::Starting -> Phase::Streaming
        // while Pane B's compressed snapshot is still in flight.
        let t1 = t0 + Duration::from_millis(55);
        fc.on_ack_at(64 * KIB, 0, false, t1);
        let after_pane_a = fc.window();
        assert!(after_pane_a <= 256 * KIB, "64 KiB raw output over 55 ms must not grow window");

        // 55 ms later, Pane B acknowledges its 2 MiB compressed snapshot. Even though
        // a fresh streaming epoch was opened, compression attribution stays with those
        // bytes until acknowledged so the window must not double.
        let t2 = t1 + Duration::from_millis(55);
        fc.on_ack_at(2 * MIB, 2 * MIB, false, t2);
        assert_eq!(fc.window(), after_pane_a, "compressed snapshot acked in later epoch must not inflate window");
        assert_eq!(fc.state.lock().in_flight, 0, "full uncompressed bytes must still be credited out of in_flight");
    }

    #[test]
    fn brief_initial_burst_does_not_trap_slow_link_at_large_window() {
        let fc = configured(256 * KIB, 64 * KIB);
        let t0 = Instant::now();
        fc.on_rtt_at(500, t0);

        // Simulate `pv` releasing an initial 256 KiB token-bucket burst in 2 ms,
        // then throttling to 500 KB/s. Window must not overshoot during the burst.
        let mut t = t0;
        for _ in 0..4 {
            fc.on_send_at(64 * KIB, t);
            t += Duration::from_micros(500);
            fc.on_ack_at(64 * KIB, 0, false, t);
        }
        assert_eq!(fc.window(), 256 * KIB, "2 ms token-bucket burst must not grow window");

        // Now link throttles to 2 MB/s while daemon fills window and lags.
        fc.on_send_at(fc.window(), t);
        for _ in 0..6 {
            t += Duration::from_millis(50);
            fc.on_ack_at(100 * KIB, 0, true, t);
        }
        assert!(fc.window() <= 160 * KIB, "slow link must shrink quickly after initial burst, got {} KiB", fc.window() / KIB);
    }

    #[test]
    fn pre_v15_client_can_grow_but_never_shrinks_below_initial() {
        let fc = FlowControl::new();
        fc.init_if_needed(256 * KIB, 256 * KIB, false);
        let t0 = Instant::now();
        fc.on_rtt_at(1_000, t0);

        // Slow 500 KB/s link: stays at 256 KiB floor and sends no notifications.
        fc.on_send_at(256 * KIB, t0);
        assert_eq!(fc.on_ack_at(256 * KIB, 0, true, t0 + Duration::from_millis(513)), None);
        assert_eq!(fc.window(), 256 * KIB);

        // Fast link: still grows up to 8 MiB without sending v15 FlowWindow.
        let mut t = t0 + Duration::from_secs(1);
        for _ in 0..1200 {
            fc.on_send_at(64 * KIB, t);
            t += Duration::from_micros(256);
            assert_eq!(fc.on_ack_at(64 * KIB, 0, false, t), None);
        }
        assert_eq!(fc.window(), 8 * MIB);
    }

    #[test]
    fn drop_in_flight_resets_stuck_phase() {
        let fc = configured(256 * KIB, 64 * KIB);
        let t0 = Instant::now();
        fc.on_send_at(64 * KIB, t0);
        fc.on_drop_in_flight(64 * KIB);
        let st = fc.state.lock();
        assert_eq!(st.in_flight, 0);
        assert!(matches!(st.phase, Phase::Idle));
    }

    #[test]
    fn stale_min_rtt_expires_after_window() {
        let fc = configured(256 * KIB, 64 * KIB);
        let t0 = Instant::now();
        fc.on_rtt_at(2_000, t0);
        // Route moves to 60 ms at t = 40 s (second half of 60 s window).
        fc.on_rtt_at(60_000, t0 + Duration::from_secs(40));
        // Still 2 ms until 60 s elapses.
        assert_eq!(fc.state.lock().effective_min_rtt(t0 + Duration::from_secs(50)), Some(Duration::from_millis(2)));
        // After 60 s, 60 ms is promoted smoothly (still only 25 s old).
        assert_eq!(fc.state.lock().effective_min_rtt(t0 + Duration::from_secs(65)), Some(Duration::from_millis(60)));

        // If both primary (t = 0) and candidate (t = 40 s) are expired at t = 110 s,
        // the 70 s old candidate must be discarded on the very first lookup.
        let fc2 = configured(256 * KIB, 64 * KIB);
        fc2.on_rtt_at(2_000, t0);
        fc2.on_rtt_at(60_000, t0 + Duration::from_secs(40));
        assert_eq!(fc2.state.lock().effective_min_rtt(t0 + Duration::from_secs(110)), None);
    }
}
