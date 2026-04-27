//! Main control loop: pull lux from the sensor, decide targets, drive backlights,
//! while respecting idle state, manual override, and IPC commands.

use anyhow::Result;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

use crate::config::{Channel, Config};
use crate::curve;
use crate::idle::IdleEvent;
use crate::ipc::{Command, StatusReply};
use crate::output::{Backlight, ChannelKind};
use crate::sensor::{LuxSample, Sensor};

struct State {
    cfg: Config,
    display: Option<Backlight>,
    keyboard: Option<Backlight>,
    last_lux: f32,
    last_targets: ChannelTargets,
    paused_until: Option<Instant>,
    override_until: Option<Instant>,
    override_lux_anchor: Option<f32>,
    nudge_pct: i32,
    idle: bool,
    idle_resume_grace_until: Option<Instant>,
}

#[derive(Default, Clone, Copy)]
struct ChannelTargets {
    display: Option<f32>,
    keyboard: Option<f32>,
}

pub async fn run(cfg: Config) -> Result<()> {
    let display = if cfg.display.enabled {
        Some(Backlight::open(ChannelKind::Display, &cfg.display)?)
    } else {
        None
    };
    let keyboard = if cfg.keyboard.enabled {
        Some(Backlight::open(ChannelKind::Keyboard, &cfg.keyboard)?)
    } else {
        None
    };

    let sensor = Sensor::new(cfg.ema_alpha)?;
    let mut lux_rx = sensor.spawn(Duration::from_millis(cfg.poll_interval_ms));

    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Command>(16);
    tokio::spawn(async move {
        if let Err(e) = crate::ipc::server::run(cmd_tx).await {
            tracing::error!(error = %e, "IPC server exited");
        }
    });

    let mut idle_rx = match crate::idle::spawn(cfg.idle_timeout_ms) {
        Ok(rx) => Some(rx),
        Err(e) => {
            tracing::warn!(error = %e, "running without Wayland idle integration");
            None
        }
    };

    let mut state = State {
        cfg,
        display,
        keyboard,
        last_lux: 0.0,
        last_targets: ChannelTargets::default(),
        paused_until: None,
        override_until: None,
        override_lux_anchor: None,
        nudge_pct: 0,
        idle: false,
        idle_resume_grace_until: None,
    };

    // Prime: read initial lux & drive targets immediately.
    if lux_rx.changed().await.is_ok() {
        let sample = *lux_rx.borrow();
        if let Err(e) = apply(&mut state, sample, true).await {
            tracing::warn!(error = %e, "initial apply failed");
        }
    }

    loop {
        tokio::select! {
            biased;

            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutdown");
                return Ok(());
            }

            Some(cmd) = cmd_rx.recv() => {
                handle_command(&mut state, cmd, &lux_rx);
            }

            idle_event = async {
                match idle_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(ev) = idle_event {
                    handle_idle(&mut state, ev);
                }
            }

            res = lux_rx.changed() => {
                if res.is_err() { return Ok(()); }
                let sample = *lux_rx.borrow();
                if let Err(e) = apply(&mut state, sample, false).await {
                    tracing::warn!(error = %e, "apply failed");
                }
            }
        }
    }
}

fn handle_idle(state: &mut State, ev: IdleEvent) {
    match ev {
        IdleEvent::Idled => {
            tracing::debug!("idle: pausing writes");
            state.idle = true;
        }
        IdleEvent::Resumed => {
            tracing::debug!("idle: resuming, grace period starting");
            state.idle = false;
            state.idle_resume_grace_until =
                Some(Instant::now() + Duration::from_millis(state.cfg.idle_resume_grace_ms));
        }
    }
}

fn handle_command(
    state: &mut State,
    cmd: Command,
    lux_rx: &tokio::sync::watch::Receiver<LuxSample>,
) {
    match cmd {
        Command::Pause(seconds) => {
            state.paused_until = if seconds == 0 {
                Some(Instant::now() + Duration::from_secs(60 * 60 * 24 * 365))
            } else {
                Some(Instant::now() + Duration::from_secs(seconds))
            };
            tracing::info!(seconds, "paused");
        }
        Command::Resume => {
            state.paused_until = None;
            state.override_until = None;
            state.override_lux_anchor = None;
            tracing::info!("resumed");
        }
        Command::Nudge(delta) => {
            state.nudge_pct = (state.nudge_pct + delta).clamp(-50, 50);
            tracing::info!(nudge = state.nudge_pct, "nudged");
        }
        Command::GetStatus(reply) => {
            let sample = *lux_rx.borrow();
            let st = StatusReply {
                lux_raw: sample.raw,
                lux_smoothed: sample.smoothed,
                display_pct: state.display.as_ref().and_then(|b| b.current_pct().ok()),
                keyboard_pct: state.keyboard.as_ref().and_then(|b| b.current_pct().ok()),
                paused_until_unix: state.paused_until.map(|t| {
                    let now = Instant::now();
                    let dur = t.saturating_duration_since(now);
                    let unix_now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    unix_now + dur.as_secs()
                }),
                override_active: state.override_until.is_some(),
                idle: state.idle,
                nudge_pct: state.nudge_pct,
            };
            let _ = reply.send(st);
        }
    }
}

async fn apply(state: &mut State, sample: LuxSample, force: bool) -> Result<()> {
    let now = Instant::now();

    if state.idle {
        return Ok(());
    }
    if let Some(until) = state.idle_resume_grace_until {
        if now < until {
            return Ok(());
        }
        state.idle_resume_grace_until = None;
        // brightnessctl -r (or whatever the idle daemon did on resume) has landed.
        sync_baselines(state);
    }
    if let Some(until) = state.paused_until {
        if now < until {
            return Ok(());
        }
        state.paused_until = None;
        sync_baselines(state);
    }
    if let Some(until) = state.override_until {
        let drifted = state
            .override_lux_anchor
            .map(|anchor| {
                let pct = if anchor > 0.0 {
                    ((sample.smoothed - anchor).abs() / anchor) * 100.0
                } else {
                    f32::INFINITY
                };
                pct >= state.cfg.override_lux_drift_pct
            })
            .unwrap_or(false);
        if now >= until || drifted {
            tracing::info!(drifted, "override expired, resuming auto control");
            state.override_until = None;
            state.override_lux_anchor = None;
            sync_baselines(state);
            // Force re-application so we ramp from the user's value to our target.
            state.last_targets = ChannelTargets::default();
        } else {
            return Ok(());
        }
    }

    // Detect external changes (manual sliders, brightnessctl, etc.) → enter override.
    if let Some(b) = &state.display {
        if b.detect_external_change()? {
            tracing::info!("external display brightness change detected → override");
            state.override_until = Some(now + Duration::from_secs(state.cfg.override_timeout_s));
            state.override_lux_anchor = Some(sample.smoothed);
            return Ok(());
        }
    }
    if let Some(b) = &state.keyboard {
        if b.detect_external_change()? {
            tracing::info!("external keyboard brightness change detected → override");
            state.override_until = Some(now + Duration::from_secs(state.cfg.override_timeout_s));
            state.override_lux_anchor = Some(sample.smoothed);
            return Ok(());
        }
    }

    let display_target = state
        .cfg
        .display
        .enabled
        .then(|| compute_target(&state.cfg.display, sample.smoothed, state.nudge_pct));
    let keyboard_target = state
        .cfg
        .keyboard
        .enabled
        .then(|| compute_target(&state.cfg.keyboard, sample.smoothed, 0));

    let needs_apply = force
        || target_changed(
            state.last_targets.display,
            display_target,
            state.cfg.display.hysteresis_pct,
        )
        || target_changed(
            state.last_targets.keyboard,
            keyboard_target,
            state.cfg.keyboard.hysteresis_pct,
        );

    if !needs_apply {
        return Ok(());
    }

    let dur = Duration::from_millis(state.cfg.ramp_duration_ms);
    let steps = state.cfg.ramp_steps;

    if let (Some(b), Some(t)) = (&mut state.display, display_target) {
        b.ramp_to(t, dur, steps).await?;
    }
    if let (Some(b), Some(t)) = (&mut state.keyboard, keyboard_target) {
        b.ramp_to(t, dur, steps).await?;
    }

    state.last_targets = ChannelTargets {
        display: display_target,
        keyboard: keyboard_target,
    };
    state.last_lux = sample.smoothed;
    Ok(())
}

fn compute_target(channel: &Channel, lux: f32, nudge_pct: i32) -> f32 {
    let base = curve::target_pct(channel, lux);
    if base == 0.0 {
        return 0.0;
    }
    (base + nudge_pct as f32).clamp(channel.min_pct, 100.0)
}

/*
 * Adopt the user-set value as our baseline.
 * Without this we'd see the user's value as an "external change" again
 * on the next tick and immediately re-enter override.
 */
fn sync_baselines(state: &mut State) {
    if let Some(b) = &mut state.display {
        if let Err(e) = b.sync_last_written() {
            tracing::warn!(error = %e, "display sync_last_written failed");
        }
    }
    if let Some(b) = &mut state.keyboard {
        if let Err(e) = b.sync_last_written() {
            tracing::warn!(error = %e, "keyboard sync_last_written failed");
        }
    }
}

fn target_changed(prev: Option<f32>, next: Option<f32>, hysteresis: f32) -> bool {
    match (prev, next) {
        (None, Some(_)) => true,
        (Some(_), None) => true,
        (None, None) => false,
        (Some(a), Some(b)) => (a - b).abs() >= hysteresis,
    }
}
