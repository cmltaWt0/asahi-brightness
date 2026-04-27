//! Wayland `ext-idle-notify-v1` integration.
//!
//! Subscribes to idle notifications from the running compositor (Hyprland, niri,
//! sway, kwin, …) and forwards Idled / Resumed events to the daemon. While idle
//! we stop driving the backlight so the user's idle daemon (hypridle, swayidle,
//! kanshi…) owns the screen, then resume cleanly after a small grace period so
//! `brightnessctl -r` has time to land.

use anyhow::{anyhow, Result};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use wayland_client::{protocol::wl_registry, Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};

#[derive(Debug, Clone, Copy)]
pub enum IdleEvent {
    Idled,
    Resumed,
}

struct State {
    notifier: Option<ExtIdleNotifierV1>,
    seat: Option<wayland_client::protocol::wl_seat::WlSeat>,
    notification: Option<ExtIdleNotificationV1>,
    tx: mpsc::UnboundedSender<IdleEvent>,
    timeout_ms: u32,
}

pub fn spawn(timeout_ms: u32) -> Result<mpsc::UnboundedReceiver<IdleEvent>> {
    let conn = Connection::connect_to_env().map_err(|e| {
        anyhow!("connect to wayland display failed: {e}. Idle integration disabled.")
    })?;
    let (tx, rx) = mpsc::unbounded_channel();

    std::thread::Builder::new()
        .name("wayland-idle".into())
        .spawn(move || {
            if let Err(e) = run_loop(conn, tx, timeout_ms) {
                tracing::warn!(error = %e, "Wayland idle loop exited");
            }
        })?;

    Ok(rx)
}

fn run_loop(conn: Connection, tx: mpsc::UnboundedSender<IdleEvent>, timeout_ms: u32) -> Result<()> {
    let display = conn.display();
    let mut event_queue: EventQueue<State> = conn.new_event_queue();
    let qh = event_queue.handle();

    let _registry = display.get_registry(&qh, ());

    let state = Arc::new(Mutex::new(State {
        notifier: None,
        seat: None,
        notification: None,
        tx,
        timeout_ms,
    }));

    // Initial roundtrip to populate globals.
    let mut s = state.lock().unwrap();
    event_queue.roundtrip(&mut s)?;
    drop(s);

    // After registry binds, set up notification.
    {
        let mut s = state.lock().unwrap();
        if let (Some(notifier), Some(seat)) = (s.notifier.clone(), s.seat.clone()) {
            let n = notifier.get_idle_notification(s.timeout_ms, &seat, &qh, ());
            s.notification = Some(n);
        } else {
            anyhow::bail!(
                "compositor does not advertise ext-idle-notify-v1; idle integration disabled"
            );
        }
    }

    loop {
        let mut s = state.lock().unwrap();
        event_queue.blocking_dispatch(&mut s)?;
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_seat" => {
                    let seat = registry.bind::<wayland_client::protocol::wl_seat::WlSeat, _, _>(
                        name,
                        version.min(7),
                        qh,
                        (),
                    );
                    state.seat = Some(seat);
                }
                "ext_idle_notifier_v1" => {
                    let notifier =
                        registry.bind::<ExtIdleNotifierV1, _, _>(name, version.min(1), qh, ());
                    state.notifier = Some(notifier);
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wayland_client::protocol::wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_seat::WlSeat,
        _: wayland_client::protocol::wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for State {
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

impl Dispatch<ExtIdleNotificationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => {
                let _ = state.tx.send(IdleEvent::Idled);
            }
            ext_idle_notification_v1::Event::Resumed => {
                let _ = state.tx.send(IdleEvent::Resumed);
            }
            _ => {}
        }
    }
}
