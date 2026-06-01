//! Ambient light sensor: discover the AOP ALS in IIO sysfs and read smoothed lux.
//!
//! Reactor model: no background task, no watch channel. The reactor owns a
//! `Sensor` and calls `sample()` once per sensor-timerfd tick.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

const ALS_NAME: &str = "aop-sensors-als";
const IIO_ROOT: &str = "/sys/bus/iio/devices";

pub struct Sensor {
    lux_path: PathBuf,
    alpha: f32,
    smoothed: Option<f32>,
}

#[derive(Debug, Clone, Copy)]
pub struct LuxSample {
    pub raw: f32,
    pub smoothed: f32,
}

impl Sensor {
    pub fn discover() -> Result<PathBuf> {
        let entries = std::fs::read_dir(IIO_ROOT).with_context(|| format!("reading {IIO_ROOT}"))?;
        for entry in entries.flatten() {
            let dir = entry.path();
            let Ok(name) = std::fs::read_to_string(dir.join("name")) else {
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
        let content = std::fs::read_to_string(path)?;
        Ok(content.trim().parse::<f32>()?)
    }

    /// Read the sensor once and fold it into the exponential moving average,
    /// returning both the raw reading and the smoothed value.
    pub fn sample(&mut self) -> Result<LuxSample> {
        let raw = Self::read_raw(&self.lux_path)?;
        let smoothed = match self.smoothed {
            None => raw,
            Some(prev) => self.alpha * raw + (1.0 - self.alpha) * prev,
        };
        self.smoothed = Some(smoothed);
        Ok(LuxSample { raw, smoothed })
    }

    /// Last smoothed value, or 0.0 if we haven't sampled yet (for status replies).
    pub fn last_smoothed(&self) -> f32 {
        self.smoothed.unwrap_or(0.0)
    }
}
