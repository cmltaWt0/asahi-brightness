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
    display_override: Option<ChannelOverride>,
    keyboard_override: Option<ChannelOverride>,
    nudge_pct: i32,
    idle: bool,
    idle_resume_grace_until: Option<Instant>,
}

#[derive(Default, Clone, Copy)]
struct ChannelTargets {
    display: Option<f32>,
    keyboard: Option<f32>,
}

#[derive(Clone, Copy)]
struct ChannelOverride {
    until: Instant,
    lux_anchor: f32,
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
        if let Err(err) = crate::ipc::server::run(cmd_tx).await {
            tracing::error!(error = %err, "IPC server exited");
        }
    });

    let mut idle_rx = match crate::idle::spawn(cfg.idle_timeout_ms) {
        Ok(rx) => Some(rx),
        Err(err) => {
            tracing::warn!(error = %err, "running without Wayland idle integration");
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
        display_override: None,
        keyboard_override: None,
        nudge_pct: 0,
        idle: false,
        idle_resume_grace_until: None,
    };

    // Prime: read initial lux & drive targets immediately.
    if lux_rx.changed().await.is_ok() {
        let sample = *lux_rx.borrow();
        if let Err(err) = apply(&mut state, sample, true).await {
            tracing::warn!(error = %err, "initial apply failed");
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
                if let Some(event) = idle_event {
                    handle_idle(&mut state, event);
                }
            }

            res = lux_rx.changed() => {
                if res.is_err() { return Ok(()); }
                let sample = *lux_rx.borrow();
                if let Err(err) = apply(&mut state, sample, false).await {
                    tracing::warn!(error = %err, "apply failed");
                }
            }
        }
    }
}

fn handle_idle(state: &mut State, event: IdleEvent) {
    match event {
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
            state.display_override = None;
            state.keyboard_override = None;
            sync_baselines(state);
            state.last_targets = ChannelTargets::default();
            tracing::info!("resumed");
        }
        Command::Nudge(delta) => {
            state.nudge_pct = (state.nudge_pct + delta).clamp(-50, 50);
            tracing::info!(nudge = state.nudge_pct, "nudged");
        }
        Command::GetStatus(reply) => {
            let sample = *lux_rx.borrow();
            let status = StatusReply {
                lux_raw: sample.raw,
                lux_smoothed: sample.smoothed,
                display_pct: state
                    .display
                    .as_ref()
                    .and_then(|backlight| backlight.current_pct().ok()),
                keyboard_pct: state
                    .keyboard
                    .as_ref()
                    .and_then(|backlight| backlight.current_pct().ok()),
                paused_until_unix: state.paused_until.map(|deadline| {
                    let now = Instant::now();
                    let remaining = deadline.saturating_duration_since(now);
                    let unix_now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|since_epoch| since_epoch.as_secs())
                        .unwrap_or(0);
                    unix_now + remaining.as_secs()
                }),
                display_override_active: state.display_override.is_some(),
                keyboard_override_active: state.keyboard_override.is_some(),
                idle: state.idle,
                nudge_pct: state.nudge_pct,
            };
            let _ = reply.send(status);
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
    // Per-channel override expiry. Each channel runs independently — display
    // expiring doesn't touch keyboard state and vice versa.
    let drift_pct = state.cfg.override_lux_drift_pct;
    let mut force_display = force;
    let mut force_keyboard = force;
    if expire_override(
        &mut state.display_override,
        sample.smoothed,
        drift_pct,
        now,
        "display",
    ) {
        sync_one(&mut state.display, "display");
        state.last_targets.display = None;
        force_display = true;
    }
    if expire_override(
        &mut state.keyboard_override,
        sample.smoothed,
        drift_pct,
        now,
        "keyboard",
    ) {
        sync_one(&mut state.keyboard, "keyboard");
        state.last_targets.keyboard = None;
        force_keyboard = true;
    }

    // Per-channel external-change detection. Both checked; one entering override
    // does not suppress the other.
    let timeout = Duration::from_secs(state.cfg.override_timeout_s);
    if state.display_override.is_none() {
        if let Some(backlight) = &state.display {
            if backlight.detect_external_change()? {
                tracing::info!("external display brightness change detected → display override");
                state.display_override = Some(ChannelOverride {
                    until: now + timeout,
                    lux_anchor: sample.smoothed,
                });
            }
        }
    }
    if state.keyboard_override.is_none() {
        if let Some(backlight) = &state.keyboard {
            if backlight.detect_external_change()? {
                tracing::info!("external keyboard brightness change detected → keyboard override");
                state.keyboard_override = Some(ChannelOverride {
                    until: now + timeout,
                    lux_anchor: sample.smoothed,
                });
            }
        }
    }

    // A channel currently in override yields no target, so it won't be written.
    let display_target = if state.display_override.is_some() {
        None
    } else if state.cfg.display.enabled {
        Some(compute_target(
            &state.cfg.display,
            sample.smoothed,
            state.nudge_pct,
        ))
    } else {
        None
    };
    let keyboard_target = if state.keyboard_override.is_some() {
        None
    } else if state.cfg.keyboard.enabled {
        Some(compute_target(&state.cfg.keyboard, sample.smoothed, 0))
    } else {
        None
    };

    let display_needs = display_target.is_some()
        && (force_display
            || target_changed(
                state.last_targets.display,
                display_target,
                state.cfg.display.hysteresis_pct,
            ));
    let keyboard_needs = keyboard_target.is_some()
        && (force_keyboard
            || target_changed(
                state.last_targets.keyboard,
                keyboard_target,
                state.cfg.keyboard.hysteresis_pct,
            ));

    if !display_needs && !keyboard_needs {
        return Ok(());
    }

    let dur = Duration::from_millis(state.cfg.ramp_duration_ms);
    let steps = state.cfg.ramp_steps;

    if display_needs {
        if let (Some(backlight), Some(target)) = (&mut state.display, display_target) {
            backlight.ramp_to(target, dur, steps).await?;
            state.last_targets.display = Some(target);
        }
    }
    if keyboard_needs {
        if let (Some(backlight), Some(target)) = (&mut state.keyboard, keyboard_target) {
            backlight.ramp_to(target, dur, steps).await?;
            state.last_targets.keyboard = Some(target);
        }
    }

    state.last_lux = sample.smoothed;
    Ok(())
}

/// Returns true if the override just expired (caller should sync baseline + force re-apply).
fn expire_override(
    override_slot: &mut Option<ChannelOverride>,
    lux: f32,
    drift_pct: f32,
    now: Instant,
    label: &str,
) -> bool {
    let Some(override_state) = *override_slot else {
        return false;
    };
    let drifted = if override_state.lux_anchor > 0.0 {
        ((lux - override_state.lux_anchor).abs() / override_state.lux_anchor) * 100.0 >= drift_pct
    } else {
        true
    };
    if now >= override_state.until || drifted {
        tracing::info!(
            channel = label,
            drifted,
            "override expired, resuming auto control"
        );
        *override_slot = None;
        true
    } else {
        false
    }
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
    sync_one(&mut state.display, "display");
    sync_one(&mut state.keyboard, "keyboard");
}

fn sync_one(slot: &mut Option<Backlight>, label: &str) {
    if let Some(backlight) = slot {
        if let Err(err) = backlight.sync_last_written() {
            tracing::warn!(error = %err, channel = label, "sync_last_written failed");
        }
    }
}

fn target_changed(prev: Option<f32>, next: Option<f32>, hysteresis: f32) -> bool {
    match (prev, next) {
        (None, Some(_)) => true,
        (Some(_), None) => true,
        (None, None) => false,
        (Some(prev_pct), Some(next_pct)) => (prev_pct - next_pct).abs() >= hysteresis,
    }
}
