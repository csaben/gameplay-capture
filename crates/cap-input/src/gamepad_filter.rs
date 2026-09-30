//! Change filter for polled gamepad values (pure; unit tested).

/// Minimum change in an axis (or analog trigger) value before it is logged
/// again. Axes are -1..1, so 0.02 is 1% of full travel.
pub const AXIS_CHANGE_THRESHOLD: f32 = 0.02;

/// Remembers the last *logged* value of one axis/button and decides whether a
/// newly polled value is worth logging.
///
/// Rules:
/// * the first observation is logged unless it is the rest value 0;
/// * afterwards a value is logged when it differs from the last logged value
///   by at least `threshold`;
/// * returning exactly to rest (0) is always logged if the last logged value
///   was not 0, so a released stick never looks stuck at 0.015;
/// * digital buttons (0/1) always pass because their change is 1.0.
#[derive(Debug, Clone, Copy)]
pub struct AxisFilter {
    last: Option<f32>,
    threshold: f32,
}

impl Default for AxisFilter {
    fn default() -> Self {
        Self::new(AXIS_CHANGE_THRESHOLD)
    }
}

impl AxisFilter {
    pub fn new(threshold: f32) -> Self {
        Self {
            last: None,
            threshold,
        }
    }

    /// Returns `Some(value)` if it should be logged (and records it).
    pub fn update(&mut self, value: f32) -> Option<f32> {
        if !value.is_finite() {
            return None;
        }
        let last = self.last.unwrap_or(0.0);
        let log = match self.last {
            None => value != 0.0,
            Some(_) => (value - last).abs() >= self.threshold || (value == 0.0 && last != 0.0),
        };
        if log {
            self.last = Some(value);
            Some(value)
        } else {
            if self.last.is_none() {
                self.last = Some(0.0);
            }
            None
        }
    }

    /// Last logged value (0 if nothing logged yet).
    pub fn last(&self) -> f32 {
        self.last.unwrap_or(0.0)
    }

    /// Forget state (e.g. on disconnect). Returns `true` if the last logged
    /// value was non-zero, i.e. the caller should log a return to 0.
    pub fn reset(&mut self) -> bool {
        let nonzero = self.last() != 0.0;
        self.last = None;
        nonzero
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadzone_and_rest() {
        let mut f = AxisFilter::default();
        assert_eq!(f.update(0.0), None); // rest, first sample
        assert_eq!(f.update(0.01), None); // below threshold
        assert_eq!(f.update(0.019), None);
        assert_eq!(f.update(0.03), Some(0.03));
        assert_eq!(f.update(0.045), None); // 0.015 from last logged
        assert_eq!(f.update(0.05), Some(0.05)); // 0.02 from last logged
        assert_eq!(f.update(0.04), None);
        assert_eq!(f.update(0.0), Some(0.0)); // back to rest always logged
        assert_eq!(f.update(0.0), None);
        assert_eq!(f.update(-1.0), Some(-1.0));
        assert_eq!(f.update(f32::NAN), None);
    }

    #[test]
    fn slow_drift_is_captured_in_steps() {
        let mut f = AxisFilter::default();
        let mut logged = 0;
        for i in 0..=100 {
            if f.update(i as f32 * 0.005).is_some() {
                logged += 1;
            }
        }
        // 0.5 total travel in 0.005 steps -> about 0.5/0.02 = 25 logs, not 100.
        assert!((20..=30).contains(&logged), "{logged}");
        assert!((f.last() - 0.5).abs() < 0.021);
    }

    #[test]
    fn first_nonzero_logged_and_buttons_pass() {
        let mut f = AxisFilter::default();
        assert_eq!(f.update(0.5), Some(0.5));
        let mut b = AxisFilter::default();
        assert_eq!(b.update(1.0), Some(1.0));
        assert_eq!(b.update(1.0), None);
        assert_eq!(b.update(0.0), Some(0.0));
        assert!(!b.reset());
        assert_eq!(f.last(), 0.5);
        assert!(f.reset());
        assert_eq!(f.last(), 0.0);
    }
}
