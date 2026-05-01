/// Build a sequence of intermediate values from `from` → `to` in `steps` steps (inclusive of target).
pub fn ramp(from: u32, to: u32, steps: u32) -> Vec<u32> {
    if steps == 0 || from == to {
        return vec![to];
    }
    let mut out = Vec::with_capacity(steps as usize);
    let from_f = from as f64;
    let span = to as f64 - from_f;
    for step_idx in 1..=steps {
        let frac = step_idx as f64 / steps as f64;
        out.push((from_f + span * frac).round() as u32);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_up() {
        let result = ramp(0, 100, 10);
        assert_eq!(result.last(), Some(&100));
        assert!(result.windows(2).all(|value| value[1] >= value[0]));
    }

    #[test]
    fn monotonic_down() {
        let result = ramp(100, 0, 10);
        assert_eq!(result.last(), Some(&0));
        assert!(result.windows(2).all(|value| value[1] <= value[0]));
    }

    #[test]
    fn no_change_returns_target() {
        let result = ramp(50, 50, 10);
        assert_eq!(result, vec![50]);
    }
}
