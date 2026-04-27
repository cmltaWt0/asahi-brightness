use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::watch;

const ALS_NAME: &str = "aop-sensors-als";
const IIO_ROOT: &str = "/sys/bus/iio/devices";

pub struct Sensor {
    lux_path: PathBuf,
    alpha: f32,
    smoothed: Option<f32>,
}

impl Sensor {
    pub fn discover() -> Result<PathBuf> {
        let entries = std::fs::read_dir(IIO_ROOT).with_context(|| format!("reading {IIO_ROOT}"))?;
        for entry in entries.flatten() {
            let dir = entry.path();
            let name_path = dir.join("name");
            let Ok(name) = std::fs::read_to_string(&name_path) else {
                continue;
            };
            if name.trim() == ALS_NAME {
                let lux = dir.join("in_illuminance_input");
                if lux.exists() {
                    return Ok(lux);
                }
            }
        }
        Err(anyhow!(
            "no IIO device named '{ALS_NAME}' found under {IIO_ROOT}"
        ))
    }

    pub fn new(alpha: f32) -> Result<Self> {
        let lux_path = Self::discover()?;
        tracing::info!(path = %lux_path.display(), "ALS discovered");
        Ok(Self {
            lux_path,
            alpha,
            smoothed: None,
        })
    }

    fn read_raw(path: &Path) -> Result<f32> {
        let s = std::fs::read_to_string(path)?;
        Ok(s.trim().parse::<f32>()?)
    }

    fn step(&mut self) -> Result<f32> {
        let raw = Self::read_raw(&self.lux_path)?;
        let smoothed = match self.smoothed {
            None => raw,
            Some(prev) => self.alpha * raw + (1.0 - self.alpha) * prev,
        };
        self.smoothed = Some(smoothed);
        Ok(smoothed)
    }

    /// Spawn the polling task; return a watch channel of (raw_lux, smoothed_lux).
    pub fn spawn(mut self, interval: Duration) -> watch::Receiver<LuxSample> {
        let (tx, rx) = watch::channel(LuxSample {
            raw: 0.0,
            smoothed: 0.0,
        });
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                match Sensor::read_raw(&self.lux_path) {
                    Ok(raw) => match self.step() {
                        Ok(smoothed) => {
                            let _ = tx.send(LuxSample { raw, smoothed });
                        }
                        Err(e) => tracing::warn!(error = %e, "ALS smoothing failed"),
                    },
                    Err(e) => {
                        tracing::warn!(error = %e, "ALS read failed");
                    }
                }
            }
        });
        rx
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LuxSample {
    pub raw: f32,
    pub smoothed: f32,
}
