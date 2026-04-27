use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub poll_interval_ms: u64,
    pub ramp_duration_ms: u64,
    pub ramp_steps: u32,
    pub ema_alpha: f32,
    pub override_timeout_s: u64,
    pub override_lux_drift_pct: f32,
    pub idle_timeout_ms: u32,
    pub idle_resume_grace_ms: u64,
    pub display: Channel,
    pub keyboard: Channel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub enabled: bool,
    /// sysfs leaf name. Display: `/sys/class/backlight/<device>/`. Keyboard: `/sys/class/leds/<device>/`.
    pub device: String,
    /// What sysfs class to look in: "backlight" or "leds".
    pub class: String,
    pub min_pct: f32,
    pub hysteresis_pct: f32,
    /// Hard cutoff: above this lux, channel is forced to 0 (intended for keyboard).
    pub cutoff_lux: Option<f32>,
    /// Piecewise-linear curve: vec of [lux, percent]. Must be sorted by lux ascending.
    pub curve: Vec<[f32; 2]>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            poll_interval_ms: 250,
            ramp_duration_ms: 200,
            ramp_steps: 20,
            ema_alpha: 0.2,
            override_timeout_s: 60,
            override_lux_drift_pct: 75.0,
            idle_timeout_ms: 30_000,
            idle_resume_grace_ms: 500,
            display: Channel {
                enabled: true,
                device: "apple-panel-bl".into(),
                class: "backlight".into(),
                min_pct: 5.0,
                hysteresis_pct: 1.5,
                cutoff_lux: None,
                curve: vec![
                    [0.0, 5.0],
                    [10.0, 15.0],
                    [50.0, 30.0],
                    [200.0, 55.0],
                    [600.0, 80.0],
                    [2000.0, 100.0],
                ],
            },
            keyboard: Channel {
                enabled: true,
                device: "kbd_backlight".into(),
                class: "leds".into(),
                min_pct: 0.0,
                hysteresis_pct: 2.0,
                cutoff_lux: Some(150.0),
                curve: vec![[0.0, 40.0], [20.0, 25.0], [80.0, 10.0], [150.0, 0.0]],
            },
        }
    }
}

pub fn load(explicit: Option<&Path>) -> Result<Config> {
    let path = match explicit {
        Some(p) => p.to_path_buf(),
        None => default_path()?,
    };

    if !path.exists() {
        let cfg = Config::default();
        if explicit.is_none() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let body = toml::to_string_pretty(&cfg)?;
            if std::fs::write(&path, body).is_ok() {
                tracing::info!(path = %path.display(), "wrote default config");
            }
        }
        return Ok(cfg);
    }

    let body =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config =
        toml::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;
    cfg.validate()?;
    Ok(cfg)
}

pub fn default_path() -> Result<PathBuf> {
    let dirs = xdg::BaseDirectories::with_prefix("asahi-brightness")?;
    Ok(dirs.get_config_home().join("config.toml"))
}

impl Config {
    fn validate(&self) -> Result<()> {
        for ch in [&self.display, &self.keyboard] {
            if !ch.enabled {
                continue;
            }
            anyhow::ensure!(!ch.curve.is_empty(), "curve for {} is empty", ch.device);
            anyhow::ensure!(
                ch.curve.windows(2).all(|w| w[0][0] <= w[1][0]),
                "curve for {} must be sorted by lux ascending",
                ch.device
            );
        }
        anyhow::ensure!(
            (0.0..=1.0).contains(&self.ema_alpha),
            "ema_alpha must be in [0,1]"
        );
        anyhow::ensure!(self.ramp_steps > 0, "ramp_steps must be > 0");
        Ok(())
    }
}
