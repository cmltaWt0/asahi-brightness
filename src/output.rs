use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;

use crate::config::Channel;

pub struct Backlight {
    pub max: u32,
    brightness_path: PathBuf,
    last_written: Option<u32>,
}

#[derive(Clone, Copy, Debug)]
pub enum ChannelKind {
    Display,
    Keyboard,
}

impl Backlight {
    pub fn open(_kind: ChannelKind, channel: &Channel) -> Result<Self> {
        let root = match channel.class.as_str() {
            "backlight" => PathBuf::from("/sys/class/backlight"),
            "leds" => PathBuf::from("/sys/class/leds"),
            other => anyhow::bail!("unknown sysfs class: {other}"),
        };
        let dir = root.join(&channel.device);
        anyhow::ensure!(
            dir.exists(),
            "device path does not exist: {}",
            dir.display()
        );

        let max = std::fs::read_to_string(dir.join("max_brightness"))
            .with_context(|| format!("reading max_brightness for {}", channel.device))?
            .trim()
            .parse::<u32>()?;
        anyhow::ensure!(max > 0, "max_brightness is zero for {}", channel.device);

        let brightness_path = dir.join("brightness");
        let probe = std::fs::OpenOptions::new()
            .write(true)
            .open(&brightness_path);
        if let Err(err) = probe {
            anyhow::bail!(
                "{} not writable ({}). Install the udev rule and ensure your user is in 'video'.",
                brightness_path.display(),
                err
            );
        }

        Ok(Self {
            max,
            brightness_path,
            last_written: None,
        })
    }

    pub fn read_raw(&self) -> Result<u32> {
        let raw_text = std::fs::read_to_string(&self.brightness_path)?;
        Ok(raw_text.trim().parse::<u32>()?)
    }

    /// Adopt the current sysfs value as our last-written baseline, so the next
    /// `detect_external_change` won't immediately re-fire. Call after exiting
    /// override or idle-resume grace.
    pub fn sync_last_written(&mut self) -> Result<()> {
        self.last_written = Some(self.read_raw()?);
        Ok(())
    }

    pub fn current_pct(&self) -> Result<f32> {
        let raw = self.read_raw()?;
        Ok(raw_to_pct(raw, self.max))
    }

    /// Detect external writes: someone else changed brightness if current != last_written ± 1.
    pub fn detect_external_change(&self) -> Result<bool> {
        let Some(last) = self.last_written else {
            return Ok(false);
        };
        let current = self.read_raw()?;
        Ok(current.abs_diff(last) > 1)
    }

    pub fn write_raw(&mut self, value: u32) -> Result<()> {
        let clamped = value.min(self.max);
        std::fs::write(&self.brightness_path, clamped.to_string())
            .with_context(|| format!("writing {}", self.brightness_path.display()))?;
        self.last_written = Some(clamped);
        Ok(())
    }

    /// Perform a smooth ramp from current → target_pct over `duration` in `steps` steps.
    /// Re-reads sysfs at start so external changes don't cause a jump.
    pub async fn ramp_to(&mut self, target_pct: f32, duration: Duration, steps: u32) -> Result<()> {
        let from = self.read_raw()?;
        let to = pct_to_raw(target_pct, self.max);
        if from == to {
            self.last_written = Some(to);
            return Ok(());
        }
        let path = crate::ramp::ramp(from, to, steps);
        let step_dur = duration
            .checked_div(steps)
            .unwrap_or(Duration::from_millis(10));
        for raw_value in path {
            self.write_raw(raw_value)?;
            tokio::time::sleep(step_dur).await;
        }
        Ok(())
    }
}

/*
 * Converts a raw brightness value (0..max) to a percentage (0.0..100.0).
 */
pub fn raw_to_pct(raw: u32, max: u32) -> f32 {
    (raw as f32 / max as f32) * 100.0
}

/*
 * Converts a percentage (0.0..100.0) to a raw brightness value (0..max).
 */
pub fn pct_to_raw(pct: f32, max: u32) -> u32 {
    let clamped = pct.clamp(0.0, 100.0);
    ((clamped / 100.0) * max as f32).round() as u32
}
