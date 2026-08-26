//! # Execution segments
//!
//! Deterministic workload partitioning across N cooperating nodes — the
//! primitive for horizontal scale (k6's `executionSegment` /
//! `executionSegmentSequence` options).
//!
//! A segment is a half-open interval `[from, to)` of the unit workload.
//! Node `i` runs the fraction of the workload that falls inside its
//! segment: VU counts, arrival rates, and iteration budgets are scaled by
//! the segment's length, deterministically — every node computes its own
//! share from the same inputs, with no coordination required.
//!
//! VU/iteration scaling uses k6's **telescoping** formula
//! `floor(n·to) − floor(n·from)`, so the per-node shares across a sequence
//! sum *exactly* to the original total (no lost or over-provisioned work).
//! This exact-sum property relies on every node spelling the shared
//! boundaries identically — use the same textual form (e.g. `"1/3"`, not
//! `"1/3"` on one node and `"0.333…"` on another), or pass the same
//! `executionSegmentSequence` so the sequence validation enforces it.

use crate::config::{ArrivalRateStage, ExecutionConfig, Stage};
use tropel_sdk::{Result, TropelError};

/// A deterministic workload partition: `[from, to)` of the unit interval.
///
/// Bounds are stored as exact rationals `(num, den)` — the same representation
/// k6 uses (`big.Rat`). This makes `floor(n·to) − floor(n·from)` exact,
/// avoiding the f64 silent-wrong-number bug that gives 0 VUs to one agent and
/// 2 to another when the segment fraction is not exactly representable in
/// binary floating point (e.g. `1/100`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExecutionSegment {
    from_num: i64,
    from_den: i64,
    to_num: i64,
    to_den: i64,
}

impl ExecutionSegment {
    /// Create a segment from exact rational bounds, validating
    /// `0 <= from/to < 1` and `from < to` (all in rational arithmetic,
    /// using i128 so `new_f64`'s large denominators don't overflow).
    pub fn new(from_num: i64, from_den: i64, to_num: i64, to_den: i64) -> Result<Self> {
        if from_num < 0 || from_den <= 0 || to_num < 0 || to_den <= 0 {
            return Err(TropelError::Config(format!(
                "execution segment bounds must be non-negative: \
                 {from_num}/{from_den}..{to_num}/{to_den}"
            )));
        }
        // Compare from < to: cross-multiply without f64.
        if (from_num as i128) * (to_den as i128) >= (to_num as i128) * (from_den as i128) {
            return Err(TropelError::Config(format!(
                "execution segment must be non-empty (from < to): \
                 {from_num}/{from_den}..{to_num}/{to_den}"
            )));
        }
        // from ≤ 1, to ≤ 1: cross-multiply.
        if from_num as i128 > from_den as i128 || to_num as i128 > to_den as i128 {
            return Err(TropelError::Config(format!(
                "execution segment bounds must be within [0, 1]: \
                 {from_num}/{from_den}..{to_num}/{to_den}"
            )));
        }
        Ok(Self {
            from_num,
            from_den,
            to_num,
            to_den,
        })
    }

    /// Convenience: create from f64 (used by tests that compute bounds
    /// programmatically). The f64 is converted to the closest rational
    /// `(num, den)` via `f64_to_rat`. This is lossy for fractions whose
    /// denominator is not a power of two (e.g. `1.0/3.0` → `(6004799503160661,
    /// 18014398509481984)`); the exact rational constructors from string
    /// parsing (`parse`, `parse_fraction`) should be preferred for production
    /// use.
    pub fn new_f64(from: f64, to: f64) -> Result<Self> {
        let (from_num, from_den) = Self::f64_to_rat(from);
        let (to_num, to_den) = Self::f64_to_rat(to);
        Self::new(from_num, from_den, to_num, to_den)
    }

    /// Converts an f64 to a rational `(num, den)` by multiplying by 1e12
    /// and reducing. This is exact for wholenumber decimals like `0.5` but
    /// approximate for `1.0/3.0`.
    fn f64_to_rat(v: f64) -> (i64, i64) {
        const SCALE: f64 = 1_000_000_000_000.0;
        let num = (v * SCALE).round() as i64;
        let den = SCALE as i64;
        let g = Self::gcd(num.abs(), den);
        (num / g, den / g)
    }

    fn gcd(a: i64, b: i64) -> i64 {
        if b == 0 {
            a.abs()
        } else {
            Self::gcd(b, a % b)
        }
    }

    /// Parse a single fraction like `"1/3"`, `"0.5"`, or `"0"` into an exact
    /// rational `(num, den)`.
    fn parse_fraction(s: &str) -> Result<(i64, i64)> {
        let s = s.trim();
        if s.is_empty() {
            return Err(TropelError::Config(
                "empty execution segment fraction".into(),
            ));
        }
        if let Some((num, den)) = s.split_once('/') {
            let n: i64 = num
                .trim()
                .parse()
                .map_err(|_| TropelError::Config(format!("invalid segment numerator '{num}'")))?;
            let d: i64 = den
                .trim()
                .parse()
                .map_err(|_| TropelError::Config(format!("invalid segment denominator '{den}'")))?;
            if d == 0 {
                return Err(TropelError::Config("division by zero in segment".into()));
            }
            let g = Self::gcd(n, d);
            Ok((n / g, d / g))
        } else if let Some(pct) = s.strip_suffix('%') {
            // k6 supports percentages too: `20%` → 1/5.
            let n: i64 = pct
                .trim()
                .parse()
                .map_err(|_| TropelError::Config(format!("invalid percentage '{pct}'")))?;
            let g = Self::gcd(n, 100);
            Ok((n / g, 100 / g))
        } else if s.contains('.') {
            // Decimal: convert to a rational via the fractional digits.
            let (int_part, frac_part) = s.split_once('.').unwrap();
            let int_num: i64 = int_part
                .parse()
                .map_err(|_| TropelError::Config(format!("invalid segment fraction '{s}'")))?;
            let frac_digits = frac_part.len() as u32;
            let den = 10i64.pow(frac_digits);
            let frac_num: i64 = if frac_part.is_empty() {
                0
            } else {
                frac_part
                    .parse()
                    .map_err(|_| TropelError::Config(format!("invalid segment fraction '{s}'")))?
            };
            let num = int_num * den + frac_num;
            let g = Self::gcd(num, den);
            Ok((num / g, den / g))
        } else {
            let n: i64 = s
                .parse()
                .map_err(|_| TropelError::Config(format!("invalid segment fraction '{s}'")))?;
            Ok((n, 1))
        }
    }

    /// Parse a segment spec like `"0:1/3"` (or `"1/3:2/3"`, `"0:1"`).
    /// The optional `sequence` (e.g. `"0,1/3,2/3,1"`) is validated when
    /// provided: the segment must be one of the consecutive pairs formed by
    /// the (sorted) sequence boundaries, and the sequence must start at 0 and
    /// end at 1.
    pub fn parse(segment: &str, sequence: Option<&str>) -> Result<Self> {
        let (from_s, to_s) = segment.split_once(':').ok_or_else(|| {
            TropelError::Config(format!(
                "invalid execution segment '{segment}' — expected 'from:to' e.g. '0:1/3'"
            ))
        })?;
        let (from_num, from_den) = Self::parse_fraction(from_s)?;
        let (to_num, to_den) = Self::parse_fraction(to_s)?;
        let seg = Self::new(from_num, from_den, to_num, to_den)?;

        if let Some(seq) = sequence {
            let bounds = Self::parse_sequence(seq)?;
            // The segment's bounds must be consecutive entries of the sequence.
            let from_f = seg.from();
            let to_f = seg.to();
            let from_idx = bounds.iter().position(|b| (*b - from_f).abs() < 1e-9);
            let to_idx = bounds.iter().position(|b| (*b - to_f).abs() < 1e-9);
            match (from_idx, to_idx) {
                (Some(i), Some(j)) if j == i + 1 => Ok(seg),
                (Some(i), Some(_)) => Err(TropelError::Config(format!(
                    "execution segment {from_s}..{to_s} skips sequence boundary '{}' — segments must be consecutive pairs of the sequence '{seq}'",
                    bounds[i + 1]
                ))),
                _ => Err(TropelError::Config(format!(
                    "execution segment bounds {from_s}..{to_s} not found in sequence '{seq}'"
                ))),
            }
        } else {
            Ok(seg)
        }
    }

    /// Parse and validate a sequence spec like `"0,1/3,2/3,1"`.
    /// Rules: non-empty, strictly ascending, starts at 0, ends at 1.
    /// Returns the boundaries as f64 for the (fuzzy) membership checks in
    /// `parse`; the exact rational form is preserved in the segment itself.
    pub fn parse_sequence(sequence: &str) -> Result<Vec<f64>> {
        let parts: Vec<&str> = sequence.split(',').map(str::trim).collect();
        if parts.len() < 2 {
            return Err(TropelError::Config(format!(
                "execution segment sequence needs at least 2 boundaries: '{sequence}'"
            )));
        }
        let mut bounds = Vec::with_capacity(parts.len());
        for p in parts {
            let (num, den) = Self::parse_fraction(p)?;
            bounds.push(num as f64 / den as f64);
        }
        if (bounds[0]).abs() > 1e-9 {
            return Err(TropelError::Config(format!(
                "execution segment sequence must start at 0: '{sequence}'"
            )));
        }
        if (bounds[bounds.len() - 1] - 1.0).abs() > 1e-9 {
            return Err(TropelError::Config(format!(
                "execution segment sequence must end at 1: '{sequence}'"
            )));
        }
        for w in bounds.windows(2) {
            if w[1] <= w[0] {
                return Err(TropelError::Config(format!(
                    "execution segment sequence must be strictly ascending: '{sequence}'"
                )));
            }
        }
        Ok(bounds)
    }

    /// The segment's lower bound (as f64 — for logging and backward-compat
    /// access).
    pub fn from(&self) -> f64 {
        self.from_num as f64 / self.from_den as f64
    }

    /// The segment's upper bound (as f64).
    pub fn to(&self) -> f64 {
        self.to_num as f64 / self.to_den as f64
    }

    /// The fraction of the workload this segment covers (`to - from`).
    pub fn fraction(&self) -> f64 {
        self.to() - self.from()
    }

    /// The segment's length as an exact rational `(num, den)` — `to − from`
    /// in rational arithmetic (`to_num/to_den − from_num/from_den`), reduced.
    /// k6 pre-computes `length` on construction; this is the same value,
    /// computed on demand. Used by the striped-offset algorithm.
    fn length_rat(&self) -> (i64, i64) {
        let num = self.to_num * self.from_den - self.from_num * self.to_den;
        let den = self.to_den * self.from_den;
        let g = Self::gcd(num, den);
        (num / g, den / g)
    }

    /// Scale a VU count deterministically with k6's **telescoping** formula
    /// `floor(n·to) − floor(n·from)` using **exact rational arithmetic** —
    /// k6 uses `big.Rat`; tropel uses `i128` integer division of the stored
    /// numerator/denominator pairs. This avoids the f64 silent-wrong-number
    /// bug that misassigns VUs when the segment fraction is not exactly
    /// representable in binary floating point (e.g. 100 agents / 100 VUs at
    /// `1/100` per agent).
    pub fn scale_vus(&self, vus: u32) -> u32 {
        if vus == 0 {
            return 0;
        }
        let n = vus as i128;
        let to_floor = n * self.to_num as i128 / self.to_den as i128;
        let from_floor = n * self.from_num as i128 / self.from_den as i128;
        let scaled = to_floor - from_floor;
        // to ≤ 1 ⇒ floor(n·to) ≤ n and floor(n·from) ≥ 0, so scaled ∈ [0, n].
        (scaled as u32).min(vus)
    }

    /// Scale an iteration budget deterministically with the same telescoping
    /// formula as `scale_vus` (see its doc for why).
    pub fn scale_iterations(&self, iterations: u64) -> u64 {
        if iterations == 0 {
            return 0;
        }
        let n = iterations as i128;
        let to_floor = n * self.to_num as i128 / self.to_den as i128;
        let from_floor = n * self.from_num as i128 / self.from_den as i128;
        let scaled = to_floor - from_floor;
        (scaled as u64).min(iterations)
    }

    /// Scale a rate (iterations/sec) by the segment's fraction.
    pub fn scale_rate(&self, rate: f64) -> f64 {
        rate * self.fraction()
    }

    /// Apply the segment to an execution config, returning a scaled copy
    /// that runs only this node's share of the workload. All scaling is
    /// deterministic — each node derives the same result from the same
    /// segment spec, so N nodes partition the workload with no coordination.
    pub fn apply(&self, exec: &ExecutionConfig) -> ExecutionConfig {
        match exec {
            ExecutionConfig::ConstantVus {
                vus,
                duration,
                graceful_stop,
                think_time,
            } => ExecutionConfig::ConstantVus {
                vus: self.scale_vus(*vus),
                duration: duration.clone(),
                graceful_stop: graceful_stop.clone(),
                think_time: think_time.clone(),
            },
            ExecutionConfig::RampingVus {
                stages,
                start_vus,
                graceful_ramp_down,
                graceful_stop,
                think_time,
            } => ExecutionConfig::RampingVus {
                stages: stages
                    .iter()
                    .map(|s| Stage {
                        duration: s.duration.clone(),
                        target: self.scale_vus(s.target),
                    })
                    .collect(),
                start_vus: self.scale_vus(*start_vus),
                graceful_ramp_down: graceful_ramp_down.clone(),
                graceful_stop: graceful_stop.clone(),
                think_time: think_time.clone(),
            },
            ExecutionConfig::ConstantArrivalRate {
                rate,
                time_unit,
                duration,
                pre_alloc_vus,
                max_vus,
                graceful_stop,
                think_time,
            } => ExecutionConfig::ConstantArrivalRate {
                rate: self.scale_rate(*rate),
                time_unit: time_unit.clone(),
                duration: duration.clone(),
                pre_alloc_vus: self.scale_vus(*pre_alloc_vus),
                max_vus: self.scale_vus(*max_vus),
                graceful_stop: graceful_stop.clone(),
                think_time: think_time.clone(),
            },
            ExecutionConfig::SharedIterations {
                iterations,
                max_duration,
                vus,
                graceful_stop,
                think_time,
            } => ExecutionConfig::SharedIterations {
                iterations: self.scale_iterations(*iterations),
                max_duration: max_duration.clone(),
                vus: self.scale_vus(*vus),
                graceful_stop: graceful_stop.clone(),
                think_time: think_time.clone(),
            },
            ExecutionConfig::RampingArrivalRate {
                start_rate,
                stages,
                time_unit,
                pre_alloc_vus,
                max_vus,
                graceful_stop,
                think_time,
            } => ExecutionConfig::RampingArrivalRate {
                start_rate: self.scale_rate(*start_rate),
                stages: stages
                    .iter()
                    .map(|s| ArrivalRateStage {
                        duration: s.duration.clone(),
                        target: self.scale_rate(s.target),
                    })
                    .collect(),
                time_unit: time_unit.clone(),
                pre_alloc_vus: self.scale_vus(*pre_alloc_vus),
                max_vus: self.scale_vus(*max_vus),
                graceful_stop: graceful_stop.clone(),
                think_time: think_time.clone(),
            },
            ExecutionConfig::PerVUIterations {
                vus,
                iterations,
                max_duration,
                graceful_stop,
                think_time,
            } => ExecutionConfig::PerVUIterations {
                vus: self.scale_vus(*vus),
                iterations: self.scale_iterations(*iterations),
                max_duration: max_duration.clone(),
                graceful_stop: graceful_stop.clone(),
                think_time: think_time.clone(),
            },
            ExecutionConfig::ExternallyControlled {
                vus,
                max_vus,
                duration,
                graceful_stop,
                think_time,
            } => ExecutionConfig::ExternallyControlled {
                vus: self.scale_vus(*vus),
                max_vus: self.scale_vus(*max_vus),
                duration: duration.clone(),
                graceful_stop: graceful_stop.clone(),
                think_time: think_time.clone(),
            },
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// TR-221: striped offsets (k6 execution_segment.go) — the interleaving
// primitive for distributed execution.
//
// Segment *scaling* (`scale_vus`/`scale_iterations`/`scale_rate`) tells each
// node how much work it owns. Striped *offsets* tell it WHERE in the global
// iteration sequence that work sits, so N nodes interleave their ticks into
// the exact original global rate instead of running independent (bunching)
// executors. This is k6's `ExecutionSegmentSequence` +
// `NewExecutionSegmentSequenceWrapper` + `GetStripedOffsets` +
// `SegmentedIndex` — ported 1:1, including the `big.Rat`-free integer
// arithmetic (rationals here are `i128` pairs, the same representation the
// rational-scaling fix already uses).
// ──────────────────────────────────────────────────────────────────────

/// An ordered chain of execution segments: `[0, b1), [b1, b2), ..., [b_{n-1}, 1)`.
/// k6's `ExecutionSegmentSequence`. The boundaries come from
/// `executionSegmentSequence` (`"0,1/3,2/3,1"` → three segments).
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionSegmentSequence(pub Vec<ExecutionSegment>);

impl ExecutionSegmentSequence {
    /// Greatest common divisor (module-level helper shared by the sequence).
    fn gcd(a: i64, b: i64) -> i64 {
        if b == 0 {
            a.abs()
        } else {
            Self::gcd(b, a % b)
        }
    }

    /// Lowest common denominator of all segment-length denominators —
    /// k6's `ExecutionSegmentSequence.LCD()`. The striping algorithm walks
    /// exactly `lcd` global ticks before the assignment pattern repeats.
    pub fn lcd(&self) -> i64 {
        let mut acc = self.0[0].length_rat().1;
        for seg in &self.0[1..] {
            let n = seg.length_rat().1;
            if acc == n || acc % n == 0 {
                continue;
            }
            acc *= n / Self::gcd(acc, n);
        }
        acc
    }

    /// Parse a sequence spec like `"0,1/3,2/3,1"` into segments (k6's
    /// `NewExecutionSegmentSequenceFromString`). Reuses `parse_fraction` for
    /// the boundary values.
    pub fn parse(sequence: &str) -> Result<Self> {
        let bounds = ExecutionSegment::parse_sequence(sequence)?;
        let mut segments = Vec::with_capacity(bounds.len() - 1);
        // Re-derive exact rational bounds from the parsed f64 set is lossy;
        // instead re-parse the string tokens for exactness.
        let tokens: Vec<&str> = sequence.split(',').map(str::trim).collect();
        let mut prev = ExecutionSegment::parse_fraction(tokens[0])?;
        for tok in &tokens[1..] {
            let cur = ExecutionSegment::parse_fraction(tok)?;
            segments.push(ExecutionSegment::new(prev.0, prev.1, cur.0, cur.1)?);
            prev = cur;
        }
        Ok(Self(segments))
    }
}

/// The pre-computed striping cache: the LCD plus each segment's offset list.
/// k6's `ExecutionSegmentSequenceWrapper`. Construction runs the striping
/// algorithm once; `get_striped_offsets` then returns cached values.
pub struct ExecutionSegmentSequenceWrapper {
    lcd: i64,
    offsets: Vec<Vec<i64>>,
}

impl ExecutionSegmentSequenceWrapper {
    /// Build the wrapper for a filled (full `[0,1]`) sequence, running the
    /// striping algorithm — k6's `NewExecutionSegmentSequenceWrapper`.
    pub fn new(sequence: &ExecutionSegmentSequence) -> Result<Self> {
        if sequence.0.is_empty() {
            return Err(TropelError::Config(
                "cannot build striped offsets from an empty segment sequence".into(),
            ));
        }
        // k6 panics on a non-full sequence — we error instead (invariant 2:
        // loud, not silent). Exact rational comparison: from == 0 and to == 1.
        let first = &sequence.0[0];
        let last = &sequence.0[sequence.0.len() - 1];
        let full = first.from_num == 0 && last.to_num == last.to_den;
        if !full {
            return Err(TropelError::Config(format!(
                "striped-offset sequence must be full (start at 0, end at 1): {sequence:?}"
            )));
        }
        let n = sequence.0.len();
        let lcd = sequence.lcd();
        let mut offsets: Vec<Vec<i64>> = vec![Vec::new(); n];

        // Normalize each segment's length to the LCD: 3/5 @ LCD 15 → 9/15.
        // Bigger normalized numerators are served first (k6 sorts descending)
        // so the segments that need the most ticks have the hardest time
        // landing sequentially.
        let mut items: Vec<(i64, usize)> = (0..n)
            .map(|i| {
                let (ln, ld) = sequence.0[i].length_rat();
                (ln * (lcd / ld), i)
            })
            .collect();
        // `sort_by_key` with `Reverse` is a stable descending sort, like the
        // k6 `sort.SliceStable` — ties keep the original (declaration) order.
        items.sort_by_key(|a| std::cmp::Reverse(a.0));

        let mut prev = vec![0i64; n];
        let mut chosen_counts = vec![0i64; n];

        for gi in 0..lcd {
            for (sorted_index, chosen) in chosen_counts.iter().enumerate() {
                let num = *chosen * lcd;
                let denom = items[sorted_index].0;
                if denom == 0 {
                    continue; // zero-length segment claims nothing
                }
                if gi > num / denom || (gi == num / denom && num % denom == 0) {
                    chosen_counts[sorted_index] += 1;
                    let idx = items[sorted_index].1;
                    offsets[idx].push(gi - prev[idx]);
                    prev[idx] = gi;
                    if offsets[idx].len() as i64 == denom {
                        // Wrap-around: offset from the last claim in this
                        // cycle to the first claim of the next cycle.
                        let wrap = offsets[idx][0] + lcd - gi;
                        offsets[idx].push(wrap);
                    }
                    break;
                }
            }
        }

        Ok(Self { lcd, offsets })
    }

    /// The cached least common denominator (number of global ticks in one
    /// full striping cycle).
    pub fn lcd(&self) -> i64 {
        self.lcd
    }

    /// k6's `GetStripedOffsets`: `(start, offsets, lcd)`. `start` is the
    /// first global tick this segment owns; `offsets` are the gaps from one
    /// owned tick to the next, cycling every `lcd` ticks. The caller walks
    /// the global tick space via `SegmentedIndex`.
    pub fn get_striped_offsets(&self, segment_index: usize) -> (i64, Vec<i64>, i64) {
        let offsets = &self.offsets[segment_index];
        (offsets[0], offsets[1..].to_vec(), self.lcd)
    }

    /// k6's `SegmentedIndex`: an iterator over the global tick space that
    /// yields only this segment's ticks — `(scaled, unscaled)` pairs where
    /// `unscaled` is the global iteration index and `scaled` is how many of
    /// this segment's iterations have elapsed. N nodes each run a
    /// `SegmentedIndex` for their own segment and interleave into the exact
    /// original global rate.
    pub fn segmented_index(&self, segment_index: usize) -> SegmentedIndex {
        let (start, offsets, lcd) = self.get_striped_offsets(segment_index);
        SegmentedIndex {
            start,
            lcd,
            offsets,
            scaled: 0,
            unscaled: 0,
        }
    }
}

/// k6's `SegmentedIndex` — the non-thread-safe striped iterator over the
/// global tick space. See `ExecutionSegmentSequenceWrapper::segmented_index`.
#[allow(dead_code)]
pub struct SegmentedIndex {
    start: i64,
    lcd: i64,
    offsets: Vec<i64>,
    scaled: i64,
    unscaled: i64,
}

impl SegmentedIndex {
    /// Advance to the next tick owned by this segment. First call yields
    /// `(1, start+1)` (k6 indexes the first element as 1, not 0); subsequent
    /// calls add this segment's offset to the global index.
    ///
    /// Implemented via [`Iterator`] (not a bare `next`) so the method can't be
    /// confused with the standard trait, and callers can `for (scaled,
    /// unscaled) in idx { … }`.
    fn advance(&mut self) -> (i64, i64) {
        if self.scaled == 0 {
            self.unscaled += self.start + 1;
        } else {
            self.unscaled += self.offsets[((self.scaled - 1) % self.offsets.len() as i64) as usize];
        }
        self.scaled += 1;
        (self.scaled, self.unscaled)
    }
}

impl Iterator for SegmentedIndex {
    type Item = (i64, i64);

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.advance())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fraction_forms() {
        assert_eq!(ExecutionSegment::parse_fraction("1/3").unwrap(), (1, 3));
        assert_eq!(ExecutionSegment::parse_fraction("0.5").unwrap(), (1, 2));
        assert_eq!(ExecutionSegment::parse_fraction("0").unwrap(), (0, 1));
        assert_eq!(ExecutionSegment::parse_fraction("2/3").unwrap(), (2, 3));
        assert_eq!(ExecutionSegment::parse_fraction("20%").unwrap(), (1, 5));
        assert_eq!(ExecutionSegment::parse_fraction("0.25").unwrap(), (1, 4));
        assert!(ExecutionSegment::parse_fraction("x/3").is_err());
        assert!(ExecutionSegment::parse_fraction("1/0").is_err());
    }

    #[test]
    fn parses_segment_spec() {
        let seg = ExecutionSegment::parse("0:1/3", None).unwrap();
        assert!((seg.from() - 0.0).abs() < 1e-9);
        assert!((seg.to() - 1.0 / 3.0).abs() < 1e-9);
        assert!((seg.fraction() - 1.0 / 3.0).abs() < 1e-9);

        let seg = ExecutionSegment::parse("1/3:2/3", None).unwrap();
        assert!((seg.from() - 1.0 / 3.0).abs() < 1e-9);
        assert!((seg.to() - 2.0 / 3.0).abs() < 1e-9);

        // Full workload
        let seg = ExecutionSegment::parse("0:1", None).unwrap();
        assert_eq!(seg.fraction(), 1.0);

        // Invalid: from >= to, out of range, malformed
        assert!(ExecutionSegment::parse("1/3:1/3", None).is_err());
        assert!(ExecutionSegment::parse("1:0", None).is_err());
        assert!(ExecutionSegment::parse("1/3", None).is_err());
        assert!(ExecutionSegment::parse("-1:1", None).is_err());
        assert!(ExecutionSegment::parse("0:2", None).is_err());
    }

    #[test]
    fn validates_against_sequence() {
        let seq = "0,1/3,2/3,1";
        // Consecutive pair → OK
        assert!(ExecutionSegment::parse("0:1/3", Some(seq)).is_ok());
        assert!(ExecutionSegment::parse("1/3:2/3", Some(seq)).is_ok());
        assert!(ExecutionSegment::parse("2/3:1", Some(seq)).is_ok());
        // Non-consecutive pair → Err
        assert!(ExecutionSegment::parse("0:2/3", Some(seq)).is_err());
        // Bound not in sequence → Err
        assert!(ExecutionSegment::parse("0:0.25", Some(seq)).is_err());
    }

    #[test]
    fn validates_sequence_shape() {
        assert!(ExecutionSegment::parse_sequence("0,1/3,2/3,1").is_ok());
        assert!(ExecutionSegment::parse_sequence("0,1").is_ok());
        // Must start at 0
        assert!(ExecutionSegment::parse_sequence("1/3,1").is_err());
        // Must end at 1
        assert!(ExecutionSegment::parse_sequence("0,1/3").is_err());
        // Must be ascending
        assert!(ExecutionSegment::parse_sequence("0,2/3,1/3,1").is_err());
        // At least 2 boundaries
        assert!(ExecutionSegment::parse_sequence("0").is_err());
    }

    #[test]
    fn scales_vus_and_iterations() {
        let third = ExecutionSegment::parse("0:1/3", None).unwrap();
        // 9 VUs × 1/3 = 3 (telescoping: floor(9·1/3) − floor(9·0) = 3)
        assert_eq!(third.scale_vus(9), 3);
        // 90 iterations × 1/3 = 30
        assert_eq!(third.scale_iterations(90), 30);
        // A sub-unit share floors to 0 — no `.max(1)` over-provision
        assert_eq!(third.scale_vus(1), 0);
        assert_eq!(third.scale_iterations(1), 0);
        // Zero stays zero
        assert_eq!(third.scale_vus(0), 0);
        // Rate is fractional, not floored
        assert!((third.scale_rate(6.0) - 2.0).abs() < 1e-9);

        let full = ExecutionSegment::parse("0:1", None).unwrap();
        assert_eq!(full.scale_vus(9), 9);
        assert_eq!(full.scale_iterations(90), 90);
    }

    #[test]
    fn telescoping_sums_exactly_across_segments() {
        // The whole point of telescoping: scaled counts across ALL segments
        // of a sequence sum EXACTLY to the original total. Independent
        // `floor(n·fraction)` loses work (10 VUs / 3 nodes → 3+3+3=9);
        // telescoping recovers it (3+3+4=10).
        let s1 = ExecutionSegment::parse("0:1/3", None).unwrap();
        let s2 = ExecutionSegment::parse("1/3:2/3", None).unwrap();
        let s3 = ExecutionSegment::parse("2/3:1", None).unwrap();

        // VUs 10/N3 → 3+3+4 = 10 (TODO's example)
        assert_eq!(s1.scale_vus(10) + s2.scale_vus(10) + s3.scale_vus(10), 10);
        assert_eq!(
            (s1.scale_vus(10), s2.scale_vus(10), s3.scale_vus(10)),
            (3, 3, 4)
        );

        // Iters 10/N4 → 2+3+2+3 = 10 (TODO's example)
        let q1 = ExecutionSegment::parse("0:1/4", None).unwrap();
        let q2 = ExecutionSegment::parse("1/4:1/2", None).unwrap();
        let q3 = ExecutionSegment::parse("1/2:3/4", None).unwrap();
        let q4 = ExecutionSegment::parse("3/4:1", None).unwrap();
        assert_eq!(
            q1.scale_iterations(10)
                + q2.scale_iterations(10)
                + q3.scale_iterations(10)
                + q4.scale_iterations(10),
            10
        );
        assert_eq!(
            (
                q1.scale_iterations(10),
                q2.scale_iterations(10),
                q3.scale_iterations(10),
                q4.scale_iterations(10)
            ),
            (2, 3, 2, 3)
        );

        // VUs 2/N3 → 0+1+1 = 2 (no `.max(1)` over-provision)
        assert_eq!(
            (s1.scale_vus(2), s2.scale_vus(2), s3.scale_vus(2)),
            (0, 1, 1)
        );

        // Arbitrary total sums back to itself for all segment counts
        for vus in [1u32, 7, 100, 999] {
            for n in [1usize, 2, 3, 7] {
                let mut total = 0u64;
                for i in 0..n {
                    let seg =
                        ExecutionSegment::new_f64(i as f64 / n as f64, (i + 1) as f64 / n as f64)
                            .unwrap();
                    total += seg.scale_vus(vus) as u64;
                }
                assert_eq!(total, vus as u64, "VUs {vus} across {n} segments");
            }
        }
    }

    /// TR-221: the register's exact claim — 100 agents / 100 VUs must give
    /// EACH agent exactly 1 VU. The old f64 telescoping
    /// (`floor(100·to) − floor(100·from)`) misread `0.03`/`0.04` boundaries
    /// (f64 `100·0.03 = 2.999…` → floor 2 while `100·0.04 = 4.000…` → floor 4),
    /// so segment 2 got 0 VUs and segment 3 got 2. Exact rationals fix it.
    #[test]
    fn hundred_agents_hundred_vus_is_one_each() {
        let mut total = 0u64;
        let mut min_vus = u32::MAX;
        let mut max_vus = 0u32;
        for i in 0..100u64 {
            let seg = ExecutionSegment::parse(&format!("{i}/100:{}/100", i + 1), None).unwrap();
            let scaled = seg.scale_vus(100);
            total += scaled as u64;
            min_vus = min_vus.min(scaled);
            max_vus = max_vus.max(scaled);
            assert_eq!(
                scaled, 1,
                "agent {i} of 100 must get exactly 1 VU from 100 VUs (rational scaling), got {scaled}"
            );
        }
        assert_eq!(total, 100, "sum must be exactly 100");
        assert_eq!(min_vus, 1);
        assert_eq!(max_vus, 1);
    }

    #[test]
    fn apply_scales_execution_config() {
        let third = ExecutionSegment::parse("0:1/3", None).unwrap();

        let exec = ExecutionConfig::ConstantVus {
            vus: 9,
            duration: "30s".into(),
            graceful_stop: Some("30s".into()),
            think_time: Default::default(),
        };
        let scaled = third.apply(&exec);
        match scaled {
            ExecutionConfig::ConstantVus { vus, .. } => assert_eq!(vus, 3),
            _ => panic!("wrong variant"),
        }

        let exec = ExecutionConfig::SharedIterations {
            iterations: 90,
            max_duration: Some("60s".into()),
            vus: 9,
            graceful_stop: Some("30s".into()),
            think_time: Default::default(),
        };
        let scaled = third.apply(&exec);
        match scaled {
            ExecutionConfig::SharedIterations {
                iterations, vus, ..
            } => {
                assert_eq!(iterations, 30);
                assert_eq!(vus, 3);
            }
            _ => panic!("wrong variant"),
        }

        let exec = ExecutionConfig::RampingVus {
            stages: vec![
                Stage {
                    duration: "1m".into(),
                    target: 10,
                },
                Stage {
                    duration: "1m".into(),
                    target: 20,
                },
            ],
            start_vus: 3,
            graceful_ramp_down: None,
            graceful_stop: None,
            think_time: Default::default(),
        };
        let scaled = third.apply(&exec);
        match scaled {
            ExecutionConfig::RampingVus {
                stages, start_vus, ..
            } => {
                assert_eq!(start_vus, 1);
                assert_eq!(stages[0].target, 3); // 10/3 = 3.33 → 3
                assert_eq!(stages[1].target, 6); // 20/3 = 6.67 → 6
            }
            _ => panic!("wrong variant"),
        }

        let exec = ExecutionConfig::ConstantArrivalRate {
            rate: 9.0,
            time_unit: "1s".into(),
            duration: "30s".into(),
            pre_alloc_vus: 3,
            max_vus: 9,
            graceful_stop: Some("30s".into()),
            think_time: Default::default(),
        };
        let scaled = third.apply(&exec);
        match scaled {
            ExecutionConfig::ConstantArrivalRate {
                rate,
                pre_alloc_vus,
                max_vus,
                ..
            } => {
                assert!((rate - 3.0).abs() < 1e-9); // 9/3 = 3
                assert_eq!(pre_alloc_vus, 1);
                assert_eq!(max_vus, 3);
            }
            _ => panic!("wrong variant"),
        }

        let exec = ExecutionConfig::RampingArrivalRate {
            start_rate: 3.0,
            stages: vec![ArrivalRateStage {
                duration: "1m".into(),
                target: 9.0,
            }],
            time_unit: "1s".into(),
            pre_alloc_vus: 3,
            max_vus: 9,
            graceful_stop: None,
            think_time: Default::default(),
        };
        let scaled = third.apply(&exec);
        match scaled {
            ExecutionConfig::RampingArrivalRate {
                start_rate,
                stages,
                pre_alloc_vus,
                max_vus,
                ..
            } => {
                assert!((start_rate - 1.0).abs() < 1e-9); // 3/3 = 1
                assert!((stages[0].target - 3.0).abs() < 1e-9); // 9/3 = 3
                assert_eq!(pre_alloc_vus, 1);
                assert_eq!(max_vus, 3);
            }
            _ => panic!("wrong variant"),
        }

        let exec = ExecutionConfig::PerVUIterations {
            vus: 9,
            iterations: 90,
            max_duration: Some("60s".into()),
            graceful_stop: Some("30s".into()),
            think_time: Default::default(),
        };
        let scaled = third.apply(&exec);
        match scaled {
            ExecutionConfig::PerVUIterations {
                vus, iterations, ..
            } => {
                assert_eq!(vus, 3);
                assert_eq!(iterations, 30);
            }
            _ => panic!("wrong variant"),
        }
    }

    /// TR-221: k6's 50%/25%/25% example from execution_segment.go's doc
    /// comment. 3 instances: 0:1/2, 1/2:3/4, 3/4:1. Combined they MUST
    /// produce every global tick exactly once, interleaved.
    #[test]
    fn striped_offsets_three_segments_cover_every_tick() {
        let seq = ExecutionSegmentSequence::parse("0,1/2,3/4,1").unwrap();
        let wrapper = ExecutionSegmentSequenceWrapper::new(&seq).unwrap();

        // The sequence is 3 segments of length 1/2, 1/4, 1/4.
        // LCD of denominators {2, 4, 4} = 4.
        assert_eq!(wrapper.lcd, 4);

        // Walk 100 global ticks and verify every tick is claimed by exactly
        // one segment.
        let mut covered = [false; 100];
        for si in 0..3 {
            let mut idx = wrapper.segmented_index(si);
            loop {
                let (_scaled, unscaled) = idx.next().expect("segmented index is unbounded");
                if unscaled > 100 {
                    break;
                }
                let gu = unscaled as usize - 1; // k6's index is 1-based
                if gu < 100 {
                    assert!(
                        !covered[gu],
                        "global tick {} claimed by more than one segment",
                        gu
                    );
                    covered[gu] = true;
                }
            }
        }
        for (gi, c) in covered.iter().enumerate() {
            assert!(c, "global tick {} was never claimed", gi);
        }
    }

    /// TR-221: the 50%/25%/25% example's exact writer output:
    /// Instance 1 (0:1/2) owns ticks: 0, 2, 4, 6, 8, 10, ...
    /// Instance 2 (1/2:3/4) owns: 1, 5, 9, 13, ...
    /// Instance 3 (3/4:1) owns: 3, 7, 11, 15, ...
    #[test]
    fn striped_offsets_exact_example_from_k6_doc() {
        let seq = ExecutionSegmentSequence::parse("0,1/2,3/4,1").unwrap();
        let wrapper = ExecutionSegmentSequenceWrapper::new(&seq).unwrap();

        let (start, offsets, lcd) = wrapper.get_striped_offsets(0);
        assert_eq!(lcd, 4);
        assert_eq!(start, 0);
        // offsets = [2, 2] (one LCD cycle of 4 ticks: claim at 0, then +2 → 2, then +2 → 4 = wrap)
        assert_eq!(offsets, vec![2, 2]);

        let (start, offsets, lcd) = wrapper.get_striped_offsets(1);
        assert_eq!(lcd, 4);
        assert_eq!(start, 1);
        assert_eq!(offsets, vec![4]);

        let (start, offsets, lcd) = wrapper.get_striped_offsets(2);
        assert_eq!(lcd, 4);
        assert_eq!(start, 3);
        assert_eq!(offsets, vec![4]);

        // Walk 20 ticks for segment 0 (50%): 0, 2, 4, 6, ... 18.
        let mut si = wrapper.segmented_index(0);
        for expect_unscaled in [1i64, 3, 5, 7, 9, 11, 13, 15, 17, 19] {
            let (scaled, unscaled) = si.next().expect("segmented index is unbounded");
            assert_eq!(unscaled, expect_unscaled, "segment 0 tick {scaled}");
        }

        // Segment 1 (25%): 1, 5, 9, 13, 17.
        let mut si = wrapper.segmented_index(1);
        for expect_unscaled in [2i64, 6, 10, 14, 18] {
            let (scaled, unscaled) = si.next().expect("segmented index is unbounded");
            assert_eq!(unscaled, expect_unscaled, "segment 1 tick {scaled}");
        }

        // Segment 2 (25%): 3, 7, 11, 15, 19.
        let mut si = wrapper.segmented_index(2);
        for expect_unscaled in [4i64, 8, 12, 16, 20] {
            let (scaled, unscaled) = si.next().expect("segmented index is unbounded");
            assert_eq!(unscaled, expect_unscaled, "segment 2 tick {scaled}");
        }
    }

    /// TR-221: 100 equal segments (each 1/100). Every segment should own
    /// exactly 1 of every 100 ticks, offset = i.
    #[test]
    fn striped_offsets_hundred_equal_segments() {
        let tokens: Vec<String> = (0..=100u64).map(|i| format!("{i}/100")).collect();
        let seq_str = tokens.join(",");
        let seq = ExecutionSegmentSequence::parse(&seq_str).unwrap();
        let wrapper = ExecutionSegmentSequenceWrapper::new(&seq).unwrap();

        assert_eq!(wrapper.lcd, 100);

        for si in 0..100 {
            let (start, offsets, lcd) = wrapper.get_striped_offsets(si);
            assert_eq!(lcd, 100);
            assert_eq!(start, si as i64, "segment {si} start");
            assert_eq!(offsets, vec![100], "segment {si} single offset"); // one claim per 100 ticks
        }
    }

    /// TR-221: Unequal 3 instances (1/2, 1/3, 1/6). LCD = 6. Verify the
    /// striping covers every tick.
    #[test]
    fn striped_offsets_three_unequal_segments() {
        // 1/2 = 3/6, 1/3 = 2/6, 1/6 = 1/6 @ LCD 6
        let seq = ExecutionSegmentSequence::parse("0,1/2,5/6,1").unwrap();
        let wrapper = ExecutionSegmentSequenceWrapper::new(&seq).unwrap();

        // Segment 0 (3/6): 3 of 6 ticks
        let (start, offsets, lcd) = wrapper.get_striped_offsets(0);
        assert_eq!(lcd, 6);
        assert_eq!(start, 0);
        assert_eq!(offsets.len(), 3);

        // Segment 1 (2/6): 2 of 6 ticks
        let (start, offsets, lcd) = wrapper.get_striped_offsets(1);
        assert_eq!(lcd, 6);
        assert_eq!(offsets.len(), 2);
        assert!(start < 6, "segment 1 must start inside the first cycle");

        // Segment 2 (1/6): 1 of 6 ticks
        let (start, offsets, lcd) = wrapper.get_striped_offsets(2);
        assert_eq!(lcd, 6);
        assert_eq!(offsets.len(), 1);
        assert!(start < 6, "segment 2 must start inside the first cycle");

        // Walk 24 ticks and verify full coverage
        let mut covered = [false; 24];
        for si in 0..3 {
            let mut idx = wrapper.segmented_index(si);
            loop {
                let (_scaled, unscaled) = idx.next().expect("segmented index is unbounded");
                if unscaled > 24 {
                    break;
                }
                let gu = unscaled as usize - 1;
                if gu < 24 {
                    assert!(!covered[gu], "tick {gu} claimed twice");
                    covered[gu] = true;
                }
            }
        }
        for (gi, c) in covered.iter().enumerate() {
            assert!(c, "global tick {gi} was never claimed");
        }
    }
}
