use crate::config::Channel;

/// Map lux to target percent using piecewise-linear interpolation.
/// Returns 0.0 if the channel is above its hard cutoff.
pub fn target_pct(channel: &Channel, lux: f32) -> f32 {
    if let Some(cut) = channel.cutoff_lux {
        if lux >= cut {
            return 0.0;
        }
    }
    let curve = &channel.curve;
    if lux <= curve[0][0] {
        return curve[0][1].max(channel.min_pct);
    }
    if lux >= curve[curve.len() - 1][0] {
        return curve[curve.len() - 1][1];
    }
    for w in curve.windows(2) {
        let (x0, y0) = (w[0][0], w[0][1]);
        let (x1, y1) = (w[1][0], w[1][1]);
        if lux >= x0 && lux <= x1 {
            let t = if x1 == x0 {
                0.0
            } else {
                (lux - x0) / (x1 - x0)
            };
            let y = y0 + t * (y1 - y0);
            return y.max(channel.min_pct);
        }
    }
    channel.min_pct
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Channel;

    fn ch(curve: Vec<[f32; 2]>, cutoff: Option<f32>, min: f32) -> Channel {
        Channel {
            enabled: true,
            device: "test".into(),
            class: "backlight".into(),
            min_pct: min,
            hysteresis_pct: 1.0,
            cutoff_lux: cutoff,
            curve,
        }
    }

    #[test]
    fn interpolates_midpoint() {
        let c = ch(vec![[0.0, 0.0], [100.0, 100.0]], None, 0.0);
        assert!((target_pct(&c, 50.0) - 50.0).abs() < 1e-3);
    }

    #[test]
    fn clamps_min_pct() {
        let c = ch(vec![[0.0, 0.0], [100.0, 100.0]], None, 5.0);
        assert!((target_pct(&c, 0.0) - 5.0).abs() < 1e-3);
    }

    #[test]
    fn applies_cutoff() {
        let c = ch(vec![[0.0, 50.0], [100.0, 50.0]], Some(150.0), 0.0);
        assert_eq!(target_pct(&c, 200.0), 0.0);
        assert!((target_pct(&c, 100.0) - 50.0).abs() < 1e-3);
    }

    #[test]
    fn extrapolates_to_endpoints() {
        let c = ch(vec![[10.0, 20.0], [100.0, 80.0]], None, 0.0);
        assert!((target_pct(&c, 5.0) - 20.0).abs() < 1e-3);
        assert!((target_pct(&c, 5000.0) - 80.0).abs() < 1e-3);
    }
}
