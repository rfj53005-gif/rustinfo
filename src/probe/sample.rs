//! Counter rates keep their baseline through short polls; resets start a new window.
use std::time::{Duration, Instant};

pub(super) const MIN_WINDOW: Duration = Duration::from_millis(50);

#[derive(Default)]
pub(super) struct CounterRate<const N: usize> {
    previous: Option<(Instant, [u64; N])>,
    cached: Option<[f64; N]>,
}

impl<const N: usize> CounterRate<N> {
    pub fn update(
        &mut self,
        values: [u64; N],
        now: Instant,
        wrap: [Option<u64>; N],
    ) -> Option<[f64; N]> {
        let Some((at, previous)) = self.previous else {
            self.previous = Some((now, values));
            return None;
        };
        let elapsed = now.saturating_duration_since(at);
        if elapsed < MIN_WINDOW {
            return self.cached;
        }
        let mut rates = [0.0; N];
        for i in 0..N {
            let delta = if values[i] >= previous[i] {
                Some(values[i] - previous[i])
            } else {
                wrap[i]
                    .filter(|range| *range > previous[i] && *range > values[i])
                    .map(|range| range - previous[i] + values[i])
            };
            let Some(delta) = delta else {
                self.previous = Some((now, values));
                self.cached = None;
                return None;
            };
            rates[i] = delta as f64 / elapsed.as_secs_f64();
        }
        self.previous = Some((now, values));
        self.cached = Some(rates);
        Some(rates)
    }

    pub fn invalidate(&mut self) {
        self.previous = None;
        self.cached = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rapid_polls_preserve_the_measurement_window() {
        let start = Instant::now();
        let mut rate = CounterRate::<1>::default();
        assert!(rate.update([100], start, [None]).is_none());
        assert!(rate
            .update([110], start + Duration::from_millis(10), [None])
            .is_none());
        assert_eq!(
            rate.update([200], start + Duration::from_secs(1), [None]),
            Some([100.0])
        );
        assert_eq!(
            rate.update([205], start + Duration::from_millis(1001), [None]),
            Some([100.0])
        );
    }
    #[test]
    fn reset_and_read_failures_never_create_a_rate_spike() {
        let start = Instant::now();
        let mut rate = CounterRate::<1>::default();
        rate.update([1000], start, [None]);
        assert!(rate
            .update([1], start + Duration::from_secs(1), [None])
            .is_none());
        assert_eq!(
            rate.update([11], start + Duration::from_secs(2), [None]),
            Some([10.0])
        );
        rate.invalidate();
        assert!(rate
            .update([4000], start + Duration::from_secs(3), [None])
            .is_none());
    }
    #[test]
    fn an_energy_counter_can_wrap_at_its_declared_range() {
        let start = Instant::now();
        let mut rate = CounterRate::<1>::default();
        rate.update([90], start, [Some(100)]);
        assert_eq!(
            rate.update([5], start + Duration::from_secs(1), [Some(100)]),
            Some([15.0])
        );
    }
}
