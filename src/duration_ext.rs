use chrono::TimeDelta;

/// Extension trait for creating TimeDelta from floating-point seconds
pub trait DurationExt {
    /// Create a TimeDelta from seconds as f32
    fn from_secs_f32(secs: f32) -> Self;

    /// Create a TimeDelta from seconds as f64
    fn from_secs_f64(secs: f64) -> Self;
}

impl DurationExt for TimeDelta {
    fn from_secs_f32(secs: f32) -> Self {
        Self::from_secs_f64(secs as f64)
    }

    fn from_secs_f64(secs: f64) -> Self {
        // Use div_euclid and rem_euclid to ensure nanoseconds are always positive
        // For negative durations, this ensures we get the correct signed seconds
        // and a positive fractional part
        let secs_int = secs.div_euclid(1.0) as i64;
        let secs_frac = secs.rem_euclid(1.0);

        // Convert fractional part to nanoseconds (always positive)
        let nanos = (secs_frac * 1e9) as u32;

        // Construct TimeDelta from seconds and nanoseconds
        TimeDelta::new(secs_int, nanos).unwrap_or_else(|| {
            // Fallback if construction fails (shouldn't happen with valid inputs)
            TimeDelta::zero()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_secs_f64_positive() {
        let duration = TimeDelta::from_secs_f64(1.5);
        assert_eq!(duration.as_seconds_f64(), 1.5);
    }

    #[test]
    fn test_from_secs_f64_negative() {
        let duration = TimeDelta::from_secs_f64(-1.5);
        assert_eq!(duration.as_seconds_f64(), -1.5);
    }

    #[test]
    fn test_from_secs_f64_zero() {
        let duration = TimeDelta::from_secs_f64(0.0);
        assert_eq!(duration.as_seconds_f64(), 0.0);
    }

    #[test]
    fn test_from_secs_f32_positive() {
        let duration = TimeDelta::from_secs_f32(1.5);
        assert!((duration.as_seconds_f32() - 1.5).abs() < 1e-6);
    }

    #[test]
    fn test_from_secs_f32_negative() {
        let duration = TimeDelta::from_secs_f32(-1.5);
        assert!((duration.as_seconds_f32() - (-1.5)).abs() < 1e-6);
    }
}
