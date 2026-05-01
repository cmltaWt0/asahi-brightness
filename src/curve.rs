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
    for segment in curve.windows(2) {
        let (lux_lo, pct_lo) = (segment[0][0], segment[0][1]);
        let (lux_hi, pct_hi) = (segment[1][0], segment[1][1]);
        if lux >= lux_lo && lux <= lux_hi {
            let frac = if lux_hi == lux_lo {
                0.0
            } else {
                (lux - lux_lo) / (lux_hi - lux_lo)
            };
            let pct = pct_lo + frac * (pct_hi - pct_lo);
            return pct.max(channel.min_pct);
        }
    }
    channel.min_pct
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Channel;

    fn make_channel(curve: Vec<[f32; 2]>, cutoff: Option<f32>, min: f32) -> Channel {
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
        let channel = make_channel(vec![[0.0, 0.0], [100.0, 100.0]], None, 0.0);
        assert!((target_pct(&channel, 50.0) - 50.0).abs() < 1e-3);
    }

    #[test]
    fn clamps_min_pct() {
        let channel = make_channel(vec![[0.0, 0.0], [100.0, 100.0]], None, 5.0);
        assert!((target_pct(&channel, 0.0) - 5.0).abs() < 1e-3);
    }

    #[test]
    fn applies_cutoff() {
        let channel = make_channel(vec![[0.0, 50.0], [100.0, 50.0]], Some(150.0), 0.0);
        assert_eq!(target_pct(&channel, 200.0), 0.0);
        assert!((target_pct(&channel, 100.0) - 50.0).abs() < 1e-3);
    }

    #[test]
    fn extrapolates_to_endpoints() {
        let channel = make_channel(vec![[10.0, 20.0], [100.0, 80.0]], None, 0.0);
        assert!((target_pct(&channel, 5.0) - 20.0).abs() < 1e-3);
        assert!((target_pct(&channel, 5000.0) - 80.0).abs() < 1e-3);
    }
}
