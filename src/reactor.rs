//! Single-threaded epoll reactor — the daemon's control loop, tokio-free.
//!
//! Every wait condition is a file descriptor handed to one `epoll_wait`:
//!   timerfd   sensor heartbeat (poll_interval_ms)
//!   signalfd  SIGINT/SIGTERM → clean shutdown
//!   listener  IPC unix socket (line-delimited JSON, handled inline)
//!   wayland   ext-idle-notify-v1 connection fd (optional; idle/resume)
//!   grace fd  one-shot: resume grace so the idle daemon's `brightnessctl -r` lands
//!   ramp fd   interval: eases both channels toward their targets, ~ramp_duration_ms
//!
//! One thread owns all state, so manual override / pause / idle / nudge are plain
//! struct fields the IPC and Wayland handlers mutate directly — no channels.

use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use nix::sys::signal::{SigSet, Signal};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use nix::sys::time::TimeSpec;
use nix::sys::timerfd::{ClockId, Expiration, TimerFd, TimerFlags, TimerSetTimeFlags};

use wayland_client::{
    protocol::{wl_registry, wl_seat::WlSeat},
    Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};

use crate::config::{Channel, Config};
use crate::curve;
use crate::ipc::{self, Reply, Request, StatusReply};
use crate::output::{self, Backlight, ChannelKind};
use crate::ramp;
use crate::sensor::Sensor;

const TOKEN_TIMER: u64 = 1;
const TOKEN_SIGNAL: u64 = 2;
const TOKEN_LISTENER: u64 = 3;
const TOKEN_WAYLAND: u64 = 4;
const TOKEN_GRACE: u64 = 5;
const TOKEN_RAMP: u64 = 6;

/// An active manual override: auto-control for the channel is suspended until
/// this deadline OR until lux drifts far enough from where it started.
#[derive(Clone, Copy)]
struct ChannelOverride {
    until: Instant,
    lux_anchor: f32,
}

/// Per-channel runtime: its config, the (optional) sysfs handle, the last target
/// we committed to, any active override, and an in-flight ramp step queue.
struct ChannelState {
    cfg: Channel,
    backlight: Option<Backlight>,
    last_target: Option<f32>,
    override_state: Option<ChannelOverride>,
    ramp: VecDeque<u32>,
}

impl ChannelState {
    fn open(kind: ChannelKind, cfg: Channel) -> Self {
        let backlight = if cfg.enabled {
            match Backlight::open(kind, &cfg) {
                Ok(backlight) => Some(backlight),
                Err(err) => {
                    tracing::warn!(error = %err, device = %cfg.device, "backlight unavailable");
                    None
                }
            }
        } else {
            None
        };
        Self {
            cfg,
            backlight,
            last_target: None,
            override_state: None,
            ramp: VecDeque::new(),
        }
    }

    fn detect_external(&self) -> bool {
        self.backlight
            .as_ref()
            .map(|backlight| backlight.detect_external_change().unwrap_or(false))
            .unwrap_or(false)
    }

    fn sync_baseline(&mut self) {
        if let Some(backlight) = self.backlight.as_mut() {
            if let Err(err) = backlight.sync_last_written() {
                tracing::warn!(error = %err, device = %self.cfg.device, "sync baseline failed");
            }
        }
    }

    fn current_pct(&self) -> Option<f32> {
        self.backlight
            .as_ref()
            .and_then(|backlight| backlight.current_pct().ok())
    }

    /// Queue a smooth ramp from the CURRENT raw value to `target` %. Returns true
    /// if steps were queued (so the caller arms the ramp timer).
    fn start_ramp(&mut self, target: f32, steps: u32) -> bool {
        self.last_target = Some(target);
        let Some(backlight) = self.backlight.as_ref() else {
            return false;
        };
        let from = backlight.read_raw().unwrap_or(0);
        let to = output::pct_to_raw(target, backlight.max);
        if from == to {
            return false;
        }
        self.ramp = ramp::ramp(from, to, steps).into();
        true
    }

    fn ramp_step(&mut self) {
        if let Some(value) = self.ramp.pop_front() {
            if let Some(backlight) = self.backlight.as_mut() {
                if let Err(err) = backlight.write_raw(value) {
                    tracing::warn!(error = %err, device = %self.cfg.device, "ramp write failed");
                }
            }
        }
    }

    fn ramping(&self) -> bool {
        !self.ramp.is_empty()
    }
}

/// All daemon state, owned by the one reactor thread.
struct Reactor {
    cfg: Config,
    sensor: Sensor,
    display: ChannelState,
    keyboard: ChannelState,
    raw: f32,
    smoothed: f32,
    paused_until: Option<Instant>,
    idle: bool,
    in_grace: bool,
    /// Set by the Wayland Resumed handler, consumed by the loop to arm the grace timer.
    arm_grace: bool,
    nudge_pct: i32,
    // Wayland globals — `notification` must stay alive or the subscription dies.
    notifier: Option<ExtIdleNotifierV1>,
    seat: Option<WlSeat>,
    notification: Option<ExtIdleNotificationV1>,
}

impl Reactor {
    /// Decide targets from the current smoothed lux and ease both channels toward
    /// them. Honors idle / grace / pause and per-channel manual override.
    fn apply(&mut self, ramp_fd: &TimerFd, force: bool) -> Result<()> {
        let now = Instant::now();
        if self.idle || self.in_grace {
            return Ok(());
        }
        if let Some(until) = self.paused_until {
            if now < until {
                return Ok(());
            }
            self.paused_until = None;
            self.sync_baselines();
            // Forget remembered targets so this pass re-asserts the curve even if
            // the target value is unchanged — lux may have moved while paused.
            self.display.last_target = None;
            self.keyboard.last_target = None;
        }

        let lux = self.smoothed;
        let drift = self.cfg.override_lux_drift_pct;
        let timeout = Duration::from_secs(self.cfg.override_timeout_s);
        let steps = self.cfg.ramp_steps;

        let mut force_display = force;
        let mut force_keyboard = force;

        // Expire overrides (timeout OR lux drift — drift is why this is inline).
        if expire_override(&mut self.display.override_state, lux, drift, now, "display") {
            self.display.sync_baseline();
            self.display.last_target = None;
            force_display = true;
        }
        if expire_override(
            &mut self.keyboard.override_state,
            lux,
            drift,
            now,
            "keyboard",
        ) {
            self.keyboard.sync_baseline();
            self.keyboard.last_target = None;
            force_keyboard = true;
        }

        // Per-channel external-change detection; one entering override doesn't
        // suppress the other.
        if self.display.override_state.is_none() && self.display.detect_external() {
            tracing::info!("external display change detected → display override");
            self.display.override_state = Some(ChannelOverride {
                until: now + timeout,
                lux_anchor: lux,
            });
        }
        if self.keyboard.override_state.is_none() && self.keyboard.detect_external() {
            tracing::info!("external keyboard change detected → keyboard override");
            self.keyboard.override_state = Some(ChannelOverride {
                until: now + timeout,
                lux_anchor: lux,
            });
        }

        // A channel in override yields no target. Nudge biases display only.
        let display_target = if self.display.override_state.is_some() || !self.display.cfg.enabled {
            None
        } else {
            Some(compute_target(&self.display.cfg, lux, self.nudge_pct))
        };
        let keyboard_target =
            if self.keyboard.override_state.is_some() || !self.keyboard.cfg.enabled {
                None
            } else {
                Some(compute_target(&self.keyboard.cfg, lux, 0))
            };

        let mut any_ramp = false;
        if let Some(target) = display_target {
            if force_display
                || target_changed(
                    self.display.last_target,
                    target,
                    self.display.cfg.hysteresis_pct,
                )
            {
                any_ramp |= self.display.start_ramp(target, steps);
            }
        }
        if let Some(target) = keyboard_target {
            if force_keyboard
                || target_changed(
                    self.keyboard.last_target,
                    target,
                    self.keyboard.cfg.hysteresis_pct,
                )
            {
                any_ramp |= self.keyboard.start_ramp(target, steps);
            }
        }

        if any_ramp {
            let step_dur = Duration::from_millis(self.cfg.ramp_duration_ms) / steps;
            ramp_fd.set(
                Expiration::Interval(TimeSpec::from_duration(step_dur)),
                TimerSetTimeFlags::empty(),
            )?;
        }
        Ok(())
    }

    fn sync_baselines(&mut self) {
        self.display.sync_baseline();
        self.keyboard.sync_baseline();
    }

    /// Translate one IPC request into a state mutation + reply, on this thread.
    fn handle_request(&mut self, request: Request) -> Reply {
        match request {
            Request::Status => {
                let paused_until_unix = self.paused_until.map(|deadline| {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    let unix_now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|since| since.as_secs())
                        .unwrap_or(0);
                    unix_now + remaining.as_secs()
                });
                Reply::Status(StatusReply {
                    lux_raw: self.raw,
                    lux_smoothed: self.smoothed,
                    display_pct: self.display.current_pct(),
                    keyboard_pct: self.keyboard.current_pct(),
                    paused_until_unix,
                    display_override_active: self.display.override_state.is_some(),
                    keyboard_override_active: self.keyboard.override_state.is_some(),
                    idle: self.idle,
                    nudge_pct: self.nudge_pct,
                })
            }
            Request::Pause { seconds } => {
                let span = if seconds == 0 {
                    Duration::from_secs(60 * 60 * 24 * 365)
                } else {
                    Duration::from_secs(seconds)
                };
                self.paused_until = Some(Instant::now() + span);
                tracing::info!(seconds, "paused");
                Reply::Ok
            }
            Request::Resume => {
                self.paused_until = None;
                self.display.override_state = None;
                self.keyboard.override_state = None;
                self.sync_baselines();
                self.display.last_target = None;
                self.keyboard.last_target = None;
                tracing::info!("resumed");
                Reply::Ok
            }
            Request::Nudge { delta } => {
                self.nudge_pct = (self.nudge_pct + delta).clamp(-50, 50);
                tracing::info!(nudge = self.nudge_pct, "nudged");
                Reply::Ok
            }
        }
    }
}

fn compute_target(channel: &Channel, lux: f32, nudge_pct: i32) -> f32 {
    let base = curve::target_pct(channel, lux);
    if base == 0.0 {
        return 0.0;
    }
    (base + nudge_pct as f32).clamp(channel.min_pct, 100.0)
}

fn target_changed(prev: Option<f32>, next: f32, hysteresis: f32) -> bool {
    match prev {
        None => true,
        Some(previous) => (previous - next).abs() >= hysteresis,
    }
}

/// True if the override just expired (caller syncs baseline + forces re-apply).
fn expire_override(
    slot: &mut Option<ChannelOverride>,
    lux: f32,
    drift_pct: f32,
    now: Instant,
    label: &str,
) -> bool {
    let Some(over) = *slot else {
        return false;
    };
    // Relative drift needs a non-zero anchor. In the dark (anchor ~0) every
    // reading is "infinite %" drift, so the old `else { true }` expired the
    // override on the next tick — a detect → override → expire oscillation.
    // With no meaningful anchor, fall back to the timeout alone.
    let drifted = if over.lux_anchor > 0.0 {
        ((lux - over.lux_anchor).abs() / over.lux_anchor) * 100.0 >= drift_pct
    } else {
        false
    };
    if now >= over.until || drifted {
        tracing::info!(channel = label, drifted, "override expired, resuming");
        *slot = None;
        true
    } else {
        false
    }
}

/// Read one line, parse a Request, mutate state, write the Reply. Synchronous —
/// clients send a single line and disconnect.
fn handle_ipc(reactor: &mut Reactor, mut stream: UnixStream) {
    let mut line = String::new();
    {
        let mut reader = BufReader::new(&stream);
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
    }
    let reply = match serde_json::from_str::<Request>(line.trim()) {
        Ok(request) => reactor.handle_request(request),
        Err(err) => Reply::Error(format!("bad request: {err}")),
    };
    let mut body = serde_json::to_string(&reply).unwrap_or_else(|_| "\"serialize error\"".into());
    body.push('\n');
    let _ = stream.write_all(body.as_bytes());
}

pub fn run(cfg: Config) -> Result<()> {
    let epoll = Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC)?;

    let timer_fd = TimerFd::new(ClockId::CLOCK_MONOTONIC, TimerFlags::TFD_CLOEXEC)?;
    let grace_fd = TimerFd::new(ClockId::CLOCK_MONOTONIC, TimerFlags::TFD_CLOEXEC)?;
    let ramp_fd = TimerFd::new(ClockId::CLOCK_MONOTONIC, TimerFlags::TFD_CLOEXEC)?;

    let mut mask = SigSet::empty();
    mask.add(Signal::SIGINT);
    mask.add(Signal::SIGTERM);
    mask.thread_block()?;
    let signal_fd = SignalFd::with_flags(&mask, SfdFlags::SFD_CLOEXEC)?;

    let socket = ipc::socket_path()?;
    let _ = std::fs::remove_file(&socket);
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    listener.set_nonblocking(true)?;
    tracing::info!(path = %socket.display(), "IPC socket listening");

    let sensor = Sensor::new(cfg.ema_alpha)?;
    let display = ChannelState::open(ChannelKind::Display, cfg.display.clone());
    let keyboard = ChannelState::open(ChannelKind::Keyboard, cfg.keyboard.clone());
    let poll_interval = Duration::from_millis(cfg.poll_interval_ms);
    let idle_timeout_ms = cfg.idle_timeout_ms;
    let grace_ms = cfg.idle_resume_grace_ms;

    let mut reactor = Reactor {
        cfg,
        sensor,
        display,
        keyboard,
        raw: 0.0,
        smoothed: 0.0,
        paused_until: None,
        idle: false,
        in_grace: false,
        arm_grace: false,
        nudge_pct: 0,
        notifier: None,
        seat: None,
        notification: None,
    };

    // Wayland idle integration is optional: if the compositor doesn't speak
    // ext-idle-notify-v1, we run without it (no idle hand-off).
    let mut wayland: Option<(Connection, EventQueue<Reactor>)> = match Connection::connect_to_env()
    {
        Ok(conn) => {
            let mut queue = conn.new_event_queue::<Reactor>();
            let queue_handle = queue.handle();
            conn.display().get_registry(&queue_handle, ());
            queue.roundtrip(&mut reactor)?;
            match (reactor.notifier.clone(), reactor.seat.clone()) {
                (Some(notifier), Some(seat)) => {
                    let notification =
                        notifier.get_idle_notification(idle_timeout_ms, &seat, &queue_handle, ());
                    reactor.notification = Some(notification);
                    epoll.add(&conn, EpollEvent::new(EpollFlags::EPOLLIN, TOKEN_WAYLAND))?;
                    tracing::info!(timeout_ms = idle_timeout_ms, "idle integration active");
                    Some((conn, queue))
                }
                _ => {
                    tracing::warn!("compositor lacks ext-idle-notify-v1; idle disabled");
                    None
                }
            }
        }
        Err(err) => {
            tracing::warn!(error = %err, "wayland unavailable; idle disabled");
            None
        }
    };

    epoll.add(&timer_fd, EpollEvent::new(EpollFlags::EPOLLIN, TOKEN_TIMER))?;
    epoll.add(
        &signal_fd,
        EpollEvent::new(EpollFlags::EPOLLIN, TOKEN_SIGNAL),
    )?;
    epoll.add(
        &listener,
        EpollEvent::new(EpollFlags::EPOLLIN, TOKEN_LISTENER),
    )?;
    epoll.add(&grace_fd, EpollEvent::new(EpollFlags::EPOLLIN, TOKEN_GRACE))?;
    epoll.add(&ramp_fd, EpollEvent::new(EpollFlags::EPOLLIN, TOKEN_RAMP))?;
    timer_fd.set(
        Expiration::Interval(TimeSpec::from_duration(poll_interval)),
        TimerSetTimeFlags::empty(),
    )?;

    // Prime: read once and drive targets immediately.
    match reactor.sensor.sample() {
        Ok(sample) => {
            reactor.raw = sample.raw;
            reactor.smoothed = sample.smoothed;
            if let Err(err) = reactor.apply(&ramp_fd, true) {
                tracing::warn!(error = %err, "initial apply failed");
            }
        }
        Err(err) => tracing::warn!(error = %err, "initial sample failed"),
    }

    let mut events = [EpollEvent::empty(); 16];
    'run: loop {
        if reactor.arm_grace {
            reactor.arm_grace = false;
            grace_fd.set(
                Expiration::OneShot(TimeSpec::from_duration(Duration::from_millis(grace_ms))),
                TimerSetTimeFlags::empty(),
            )?;
            reactor.in_grace = true;
        }

        // Wayland read handshake brackets epoll_wait (see ipc/idle notes).
        let read_guard = if let Some((_conn, queue)) = wayland.as_mut() {
            queue.dispatch_pending(&mut reactor)?;
            queue.flush()?;
            match queue.prepare_read() {
                Some(guard) => Some(guard),
                None => continue,
            }
        } else {
            None
        };

        let ready = epoll.wait(&mut events, EpollTimeout::NONE)?;

        let mut wayland_ready = false;
        for event in &events[..ready] {
            match event.data() {
                TOKEN_TIMER => {
                    timer_fd.wait()?;
                    match reactor.sensor.sample() {
                        Ok(sample) => {
                            reactor.raw = sample.raw;
                            reactor.smoothed = sample.smoothed;
                            if let Err(err) = reactor.apply(&ramp_fd, false) {
                                tracing::warn!(error = %err, "apply failed");
                            }
                        }
                        Err(err) => tracing::warn!(error = %err, "sensor sample failed"),
                    }
                }
                TOKEN_SIGNAL => {
                    if let Ok(Some(info)) = signal_fd.read_signal() {
                        tracing::info!(signal = info.ssi_signo, "shutdown");
                    }
                    break 'run;
                }
                TOKEN_LISTENER => loop {
                    match listener.accept() {
                        Ok((stream, _)) => handle_ipc(&mut reactor, stream),
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(err) => {
                            tracing::warn!(error = %err, "IPC accept failed");
                            break;
                        }
                    }
                },
                TOKEN_WAYLAND => wayland_ready = true,
                TOKEN_GRACE => {
                    grace_fd.wait()?;
                    reactor.in_grace = false;
                    reactor.sync_baselines();
                    // Forget targets so the next sensor tick re-asserts the curve
                    // (lux may have changed during idle); otherwise the daemon
                    // thinks it is already on target and never corrects.
                    reactor.display.last_target = None;
                    reactor.keyboard.last_target = None;
                    tracing::debug!("idle resume grace elapsed");
                }
                TOKEN_RAMP => {
                    ramp_fd.wait()?;
                    reactor.display.ramp_step();
                    reactor.keyboard.ramp_step();
                    if !reactor.display.ramping() && !reactor.keyboard.ramping() {
                        ramp_fd.unset()?;
                    }
                }
                other => tracing::warn!(token = other, "unknown epoll token"),
            }
        }

        if let Some(guard) = read_guard {
            if wayland_ready {
                guard.read()?;
            } else {
                drop(guard);
            }
        }
    }

    let _ = std::fs::remove_file(&socket);
    Ok(())
}

// ── Wayland Dispatch handlers — they mutate Reactor directly (no mpsc). ──

impl Dispatch<wl_registry::WlRegistry, ()> for Reactor {
    fn event(
        reactor: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        queue_handle: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_seat" => {
                    reactor.seat =
                        Some(registry.bind::<WlSeat, _, _>(name, version.min(7), queue_handle, ()));
                }
                "ext_idle_notifier_v1" => {
                    reactor.notifier = Some(registry.bind::<ExtIdleNotifierV1, _, _>(
                        name,
                        version.min(1),
                        queue_handle,
                        (),
                    ));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for Reactor {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for Reactor {
    fn event(
        _: &mut Self,
        _: &ExtIdleNotifierV1,
        _: <ExtIdleNotifierV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for Reactor {
    fn event(
        reactor: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => {
                tracing::debug!("idle: pausing writes");
                reactor.idle = true;
            }
            ext_idle_notification_v1::Event::Resumed => {
                tracing::debug!("idle: resumed");
                reactor.idle = false;
                reactor.arm_grace = true;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs_from_now: u64, anchor: f32) -> Option<ChannelOverride> {
        // `until` is in the future; expiry should hinge on drift, not the clock.
        Some(ChannelOverride {
            until: Instant::now() + Duration::from_secs(secs_from_now),
            lux_anchor: anchor,
        })
    }

    #[test]
    fn override_in_the_dark_holds_on_drift() {
        // Regression: anchor ~0 (dark room) must NOT auto-expire on drift, else
        // a manual change oscillates detect → override → expire every tick.
        let mut slot = at(60, 0.0);
        let expired = expire_override(&mut slot, 0.0, 75.0, Instant::now(), "test");
        assert!(!expired, "override at 0 lux should hold");
        assert!(slot.is_some());
    }

    #[test]
    fn override_expires_on_large_drift() {
        // Bright room: lux moving 100 -> 300 is 200% drift, past the 75% band.
        let mut slot = at(60, 100.0);
        assert!(expire_override(
            &mut slot,
            300.0,
            75.0,
            Instant::now(),
            "test"
        ));
        assert!(slot.is_none());
    }

    #[test]
    fn override_expires_on_timeout() {
        // Deadline in the past expires regardless of anchor.
        let mut slot = Some(ChannelOverride {
            until: Instant::now() - Duration::from_secs(1),
            lux_anchor: 0.0,
        });
        assert!(expire_override(
            &mut slot,
            0.0,
            75.0,
            Instant::now(),
            "test"
        ));
    }
}
