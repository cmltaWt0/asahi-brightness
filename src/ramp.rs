/// Build a sequence of intermediate values from `from` → `to` in `steps` steps (inclusive of target).
pub fn ramp(from: u32, to: u32, steps: u32) -> Vec<u32> {
    if steps == 0 || from == to {
        return vec![to];
    }
    let mut out = Vec::with_capacity(steps as usize);
    let f = from as f64;
    let span = to as f64 - f;
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        out.push((f + span * t).round() as u32);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_up() {
        let v = ramp(0, 100, 10);
        assert_eq!(v.last(), Some(&100));
        assert!(v.windows(2).all(|w| w[1] >= w[0]));
    }

    #[test]
    fn monotonic_down() {
        let v = ramp(100, 0, 10);
        assert_eq!(v.last(), Some(&0));
        assert!(v.windows(2).all(|w| w[1] <= w[0]));
    }

    #[test]
    fn no_change_returns_target() {
        let v = ramp(50, 50, 10);
        assert_eq!(v, vec![50]);
    }
}
