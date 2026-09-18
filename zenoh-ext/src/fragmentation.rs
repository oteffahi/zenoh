//
// Copyright (c) 2022 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//
use std::time::Instant;

use zenoh::{bytes::ZBytes, internal::zerror, sample::Sample, Result as ZResult};

/// Per-source DoS cap on the number of fragments a single sample may carry.
///
/// 4096 fragments times each fragment's payload size (typically at most the
/// link MTU) bounds worst-case per-sequence-number memory while still allowing
/// very large payloads.  Users can raise this limit via
/// [`AdvancedSubscriberBuilder::max_fragments`](crate::AdvancedSubscriberBuilder::max_fragments).
pub(crate) const MAX_FRAGMENTS_DEFAULT: u32 = 4096;

/// Compute the number of fragments a `payload_len`-byte payload requires when
/// split in `size`-byte fragments, checking it fits `u32` (the wire encoding
/// of fragment metadata cannot represent more).
///
/// Subscriber-side fragment caps (for example
/// [`AdvancedSubscriberBuilder::max_fragments`](crate::AdvancedSubscriberBuilder::max_fragments),
/// defaulting to [`MAX_FRAGMENTS_DEFAULT`]) are enforced on the subscriber:
/// fragments beyond a subscriber's cap are rejected, so publishing more
/// fragments than a subscriber accepts results in that sample being dropped
/// by that subscriber.
///
/// The caller guarantees `payload_len > 0 && size > 0`.
pub(crate) fn fragment_count(payload_len: usize, size: usize) -> ZResult<u32> {
    u32::try_from(payload_len.div_ceil(size)).map_err(|_| {
        zerror!("payload of {payload_len} bytes would require more than u32::MAX fragments").into()
    })
}

/// A missing fragment range: `(start, end)` fragment-number bounds, the end
/// being `None` for the open-ended tail of a sample.
pub(crate) type FragRange = (Option<u32>, Option<u32>);

#[derive(Debug, Clone)]
pub(crate) enum FragmentedSample {
    /// A non-fragmented sample, carrying its creation instant so that
    /// `last_arrival()` never fabricates a fresh one.
    Single(Instant, Sample),
    Partial {
        frag_count: u32,
        // TODO: `from_first_fragment` eagerly allocates `vec![None; frag_count]`
        //       (~600 KB per slot at the 4096-fragment cap, scaled by `max_history_depth`),
        //       allowing a payload-less attacker to inflate memory by declaring
        //       a large `frag_count` on each first fragment.
        //       A sparse `BTreeMap<u32, Sample>`, or deferring allocation until
        //       a non-first fragment arrives, should be considered.
        frags: Vec<Option<Sample>>,
        /// Instant of the most recently accepted fragment: the fragment
        /// recovery scan uses it to detect a stalled sequential stream before
        /// querying the trailing (open-ended) missing range.
        last_arrival: Instant,
        /// Missing hole ranges already targeted by an immediate recovery
        /// query, used to avoid re-firing a query on every fragment arrival
        /// while a hole persists. The recurring recovery scan ignores this
        /// and re-queries all holes each tick as a safety net.
        queried_holes: Vec<(u32, u32)>,
    },
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) enum FragInsertError {
    InvalidFragNum { frag_count: u32, frag_num: u32 },
    InvalidFragCount { frag_count: u32 },
    CountMismatch { expected: u32, got: u32 },
    ExceedsMax { frag_count: u32, max: u32 },
}

impl FragmentedSample {
    #[inline]
    pub(crate) fn single(sample: Sample) -> Self {
        Self::Single(Instant::now(), sample)
    }

    /// Build a `FragmentedSample` from a vector of already-complete fragments.
    /// The caller guarantees all fragments are present and in-order.
    pub(crate) fn from_complete_vec(fragments: Vec<Sample>) -> Self {
        match fragments.len() {
            0 => Self::Partial {
                frag_count: 0,
                frags: Vec::new(),
                last_arrival: Instant::now(),
                queried_holes: Vec::new(),
            },
            1 => Self::Single(Instant::now(), fragments.into_iter().next().unwrap()),
            n => Self::Partial {
                frag_count: n as u32,
                frags: fragments.into_iter().map(Some).collect(),
                last_arrival: Instant::now(),
                queried_holes: Vec::new(),
            },
        }
    }

    /// Create the first slot for a fragmented sample.
    pub(crate) fn from_first_fragment(
        sample: Sample,
        frag_num: u32,
        frag_count: u32,
        max_fragments: u32,
    ) -> Result<Self, FragInsertError> {
        if frag_count == 0 {
            return Err(FragInsertError::InvalidFragCount { frag_count });
        }
        if frag_num >= frag_count {
            return Err(FragInsertError::InvalidFragNum {
                frag_count,
                frag_num,
            });
        }
        if frag_count > max_fragments {
            return Err(FragInsertError::ExceedsMax {
                frag_count,
                max: max_fragments,
            });
        }
        if frag_count == 1 {
            return Ok(Self::Single(Instant::now(), sample));
        }
        let mut frags = vec![None; frag_count as usize];
        frags[frag_num as usize] = Some(sample);
        Ok(Self::Partial {
            frag_count,
            frags,
            last_arrival: Instant::now(),
            queried_holes: Vec::new(),
        })
    }

    #[inline]
    pub(crate) fn is_complete(&self) -> bool {
        match self {
            Self::Single(..) => true,
            Self::Partial { frags, .. } => !frags.is_empty() && frags.iter().all(Option::is_some),
        }
    }

    /// Iterate over all fragments. For a `Single` sample yields one element.
    ///
    /// For a [`Self::Partial`] sample with out-of-order fragments this yields
    /// only the fragments that have arrived so far, in fragment-number order.
    #[inline]
    pub(crate) fn iter_frags(&self) -> FragsIter<'_> {
        match self {
            Self::Single(_, s) => FragsIter {
                inner: FragsIterInner::Single(Some(s)),
            },
            Self::Partial { frags, .. } => FragsIter {
                inner: FragsIterInner::Partial(frags.iter()),
            },
        }
    }

    /// Return the ranges of missing fragment numbers.
    /// For a complete or single-fragment sample the result is empty.
    pub(crate) fn missing_ranges(&self) -> Vec<FragRange> {
        match self {
            Self::Single(..) => Vec::new(),
            Self::Partial { frags, .. } => missing_ranges_impl(frags),
        }
    }

    /// Return the closed ranges of missing fragment numbers ("holes": ranges
    /// with known missing fragments both before and after them, e.g. fragment
    /// 2 in a sample whose fragments 0, 1 and 3 arrived). Sequential arrival
    /// can never fill a hole, so holes are always recoverable via queries.
    /// For a complete or single-fragment sample the result is empty.
    pub(crate) fn missing_holes(&self) -> Vec<FragRange> {
        self.missing_ranges()
            .into_iter()
            .filter(|r| r.1.is_some())
            .collect()
    }

    /// Return the open-ended trailing range of missing fragment numbers (the
    /// "tail": fragments following the highest contiguous received fragment,
    /// e.g. fragments 3.. in a sample whose fragments 0, 1 and 2 arrived).
    /// Unlike holes, the tail is normally filled by the sequential arrival of
    /// the remaining fragments: it should only be queried once the stream is
    /// deemed stalled. `None` if there is no missing tail.
    pub(crate) fn missing_tail(&self) -> Option<FragRange> {
        match self {
            Self::Single(..) => None,
            Self::Partial { frags, .. } => missing_ranges_impl(frags)
                .into_iter()
                .find(|r| r.1.is_none()),
        }
    }

    /// Return the current missing hole ranges not already covered by
    /// `queried_holes`, and record them there: the caller fires one immediate
    /// recovery query per returned range. A subsequent call — e.g. after the
    /// next fragment arrival — only returns *newly opened* or *reshaped*
    /// holes, preventing redundant queries while a hole persists. `insert`
    /// prunes `queried_holes` as fragments fill them, so a hole that shrinks
    /// (e.g. `0..1, 3..4` collapsing to `1..1`) is re-queried for its new
    /// bounds.
    pub(crate) fn new_holes(&mut self) -> Vec<FragRange> {
        let Self::Partial {
            frags,
            queried_holes,
            ..
        } = self
        else {
            return Vec::new();
        };
        let mut new = Vec::new();
        for r in missing_ranges_impl(frags) {
            if r.1.is_none()
                || queried_holes
                    .iter()
                    .any(|&(s, e)| r.0.unwrap() >= s && r.1.unwrap() <= e)
            {
                continue;
            }
            queried_holes.push((r.0.unwrap(), r.1.unwrap()));
            new.push(r);
        }
        new
    }

    /// Instant of the most recently accepted fragment: the fragment recovery
    /// scan uses it to detect a stalled sequential stream before querying the
    /// trailing (open-ended) missing range.
    pub(crate) fn last_arrival(&self) -> Instant {
        match self {
            Self::Single(created, _) => *created,
            Self::Partial { last_arrival, .. } => *last_arrival,
        }
    }

    /// Insert a fragment into this slot.
    ///
    /// Duplicate fragments overwrite previously stored ones.
    ///
    /// # Errors
    /// * `InvalidFragNum` if `frag_num >= frag_count`.
    /// * `InvalidFragCount` if `frag_count == 0`.
    /// * `CountMismatch` if the incoming `frag_count` disagrees with the slot.
    ///
    /// # Known limitation
    /// A slot whose `frag_count` is contradicted by every subsequent fragment
    /// is locked-until-gone: `CountMismatch` is returned forever. See the
    /// FIXME in `spawn_frag_recovery` for the resulting unbounded recovery
    /// query churn.
    pub(crate) fn insert(
        &mut self,
        sample: Sample,
        frag_num: u32,
        frag_count: u32,
    ) -> Result<(), FragInsertError> {
        if frag_count == 0 {
            return Err(FragInsertError::InvalidFragCount { frag_count });
        }
        if frag_num >= frag_count {
            return Err(FragInsertError::InvalidFragNum {
                frag_count,
                frag_num,
            });
        }
        match self {
            Self::Single(..) => {
                if frag_count == 1 {
                    *self = Self::Single(Instant::now(), sample);
                    return Ok(());
                }
                // A Single only exists when the sample was advertised as a
                // single fragment (or assembled from a complete vector).  A
                // later fragment claiming a different count is inconsistent.
                Err(FragInsertError::CountMismatch {
                    expected: 1,
                    got: frag_count,
                })
            }
            Self::Partial {
                frag_count: existing,
                frags,
                last_arrival,
                queried_holes,
            } => {
                if *existing != frag_count {
                    return Err(FragInsertError::CountMismatch {
                        expected: *existing,
                        got: frag_count,
                    });
                }
                frags[frag_num as usize] = Some(sample);
                *last_arrival = Instant::now();
                // Filling fragments shrinks holes: prune covered ranges so a
                // hole that shrinks (or disappears) can be re-queried.
                queried_holes.retain(|&(s, e)| s > frag_num || e < frag_num);
                Ok(())
            }
        }
    }

    /// Consume this slot and return the reassembled [`Sample`] if complete.
    pub(crate) fn into_sample(self) -> Option<Sample> {
        match self {
            Self::Single(_, s) => Some(s),
            Self::Partial { frags, .. } => {
                if !frags.iter().all(Option::is_some) {
                    return None;
                }
                let mut iter = frags.into_iter();
                let first = iter.next()??;
                let mut payload = ZBytes::writer();
                payload.append(first.payload().clone());
                for frag in iter {
                    let frag = frag?;
                    payload.append(frag.payload().clone());
                }
                Some(first.with_payload(payload.finish()))
            }
        }
    }
}

pub(crate) struct FragsIter<'a> {
    inner: FragsIterInner<'a>,
}

enum FragsIterInner<'a> {
    Single(Option<&'a Sample>),
    Partial(::std::slice::Iter<'a, Option<Sample>>),
}

impl<'a> Iterator for FragsIter<'a> {
    type Item = &'a Sample;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            FragsIterInner::Single(s) => s.take(),
            FragsIterInner::Partial(iter) => iter.find_map(|f| f.as_ref()),
        }
    }
}

fn missing_ranges_impl(frags: &[Option<Sample>]) -> Vec<FragRange> {
    let mut missing_ranges: Vec<FragRange> = vec![];
    for (i, frag) in frags.iter().enumerate() {
        if missing_ranges.is_empty() {
            if frag.is_none() {
                missing_ranges.push((Some(i as u32), None));
            }
        } else {
            let last_index = missing_ranges.len() - 1;
            if frag.is_none() {
                if missing_ranges[last_index].1.is_some() {
                    missing_ranges.push((Some(i as u32), None));
                }
            } else if missing_ranges[last_index].0.is_some()
                && missing_ranges[last_index].1.is_none()
            {
                missing_ranges[last_index].1 = Some((i - 1) as u32);
            }
        }
    }
    missing_ranges
}

#[cfg(test)]
mod tests {
    use zenoh::{
        key_expr::KeyExpr,
        sample::{FragInfo, Sample, SampleBuilder},
    };

    use super::{fragment_count, FragInsertError, FragmentedSample, MAX_FRAGMENTS_DEFAULT};

    fn make_sample(payload: &str, frag_num: u32, frag_count: u32) -> Sample {
        SampleBuilder::put(KeyExpr::try_from("test/key").unwrap(), payload)
            .frag_info(FragInfo::new(frag_count, frag_num))
            .into()
    }

    #[test]
    fn out_of_order_insert_completes() {
        let s0 = make_sample("A", 0, 3);
        let s1 = make_sample("B", 1, 3);
        let s2 = make_sample("C", 2, 3);

        let mut fs =
            FragmentedSample::from_first_fragment(s2, 2, 3, MAX_FRAGMENTS_DEFAULT).unwrap();
        assert!(!fs.is_complete());
        fs.insert(s1, 1, 3).unwrap();
        assert!(!fs.is_complete());
        fs.insert(s0, 0, 3).unwrap();
        assert!(fs.is_complete());

        let sample = fs.into_sample().unwrap();
        assert_eq!(sample.payload().try_to_string().unwrap().as_ref(), "ABC");
    }

    #[test]
    fn invalid_frag_num_ge_count() {
        let s = make_sample("A", 5, 3);
        assert!(matches!(
            FragmentedSample::from_first_fragment(s, 5, 3, MAX_FRAGMENTS_DEFAULT),
            Err(FragInsertError::InvalidFragNum {
                frag_count: 3,
                frag_num: 5,
            })
        ));
    }

    #[test]
    fn invalid_frag_count_zero() {
        let s = make_sample("A", 0, 0);
        assert!(matches!(
            FragmentedSample::from_first_fragment(s, 0, 0, MAX_FRAGMENTS_DEFAULT),
            Err(FragInsertError::InvalidFragCount { frag_count: 0 })
        ));
    }

    #[test]
    fn exceeds_max() {
        let s = make_sample("A", 0, 5000);
        assert!(matches!(
            FragmentedSample::from_first_fragment(s, 0, 5000, MAX_FRAGMENTS_DEFAULT),
            Err(FragInsertError::ExceedsMax {
                frag_count: 5000,
                max: 4096,
            })
        ));
    }

    #[test]
    fn from_complete_vec_empty() {
        let fs = FragmentedSample::from_complete_vec(Vec::new());
        assert!(!fs.is_complete());
        assert!(fs.into_sample().is_none());
    }

    #[test]
    fn from_complete_vec_single() {
        let s = make_sample("A", 0, 1);
        let fs = FragmentedSample::from_complete_vec(vec![s]);
        assert!(fs.is_complete());
        assert_eq!(
            fs.into_sample()
                .unwrap()
                .payload()
                .try_to_string()
                .unwrap()
                .as_ref(),
            "A"
        );
    }

    #[test]
    fn from_complete_vec_multi() {
        let s0 = make_sample("A", 0, 3);
        let s1 = make_sample("B", 1, 3);
        let s2 = make_sample("C", 2, 3);
        let fs = FragmentedSample::from_complete_vec(vec![s0, s1, s2]);
        assert!(fs.is_complete());
        assert_eq!(
            fs.into_sample()
                .unwrap()
                .payload()
                .try_to_string()
                .unwrap()
                .as_ref(),
            "ABC"
        );
    }

    #[test]
    fn missing_ranges() {
        let s0 = make_sample("A", 0, 5);
        let s2 = make_sample("C", 2, 5);
        let s4 = make_sample("E", 4, 5);
        let mut fs =
            FragmentedSample::from_first_fragment(s0, 0, 5, MAX_FRAGMENTS_DEFAULT).unwrap();
        fs.insert(s2, 2, 5).unwrap();
        fs.insert(s4, 4, 5).unwrap();
        let ranges = fs.missing_ranges();
        assert_eq!(ranges, vec![(Some(1), Some(1)), (Some(3), Some(3))]);

        // Holes are the closed ranges, the tail is the open-ended one.
        assert_eq!(
            fs.missing_holes(),
            vec![(Some(1), Some(1)), (Some(3), Some(3))]
        );
        assert_eq!(fs.missing_tail(), None);
    }

    #[test]
    fn missing_holes_and_tail() {
        // Fragments 0, 1 and 3 of 5 received: hole at 2, tail at 4.
        let s0 = make_sample("A", 0, 5);
        let s1 = make_sample("B", 1, 5);
        let s3 = make_sample("D", 3, 5);
        let mut fs =
            FragmentedSample::from_first_fragment(s0, 0, 5, MAX_FRAGMENTS_DEFAULT).unwrap();
        fs.insert(s1, 1, 5).unwrap();
        fs.insert(s3, 3, 5).unwrap();
        assert_eq!(fs.missing_holes(), vec![(Some(2), Some(2))]);
        assert_eq!(fs.missing_tail(), Some((Some(4), None)));
    }

    #[test]
    fn new_holes_dedup_and_reshape() {
        // Fragments 0 and 2 of 5 received: hole (1, 1).
        let s0 = make_sample("A", 0, 5);
        let s2 = make_sample("C", 2, 5);
        let mut fs =
            FragmentedSample::from_first_fragment(s0, 0, 5, MAX_FRAGMENTS_DEFAULT).unwrap();
        fs.insert(s2, 2, 5).unwrap();
        // First call reports the hole and records it.
        assert_eq!(fs.new_holes(), vec![(Some(1), Some(1))]);
        // Second call reports nothing while the hole persists.
        assert!(fs.new_holes().is_empty());
        // Fragment 3 arrives, reshaping the hole to (1, 1) still — but the
        // recorded range still covers it, so nothing new.
        fs.insert(make_sample("D", 3, 5), 3, 5).unwrap();
        assert!(fs.new_holes().is_empty());
        // Fragment 4 arrives (completing the tail): the hole is unchanged.
        fs.insert(make_sample("E", 4, 5), 4, 5).unwrap();
        assert!(fs.new_holes().is_empty());
        // Fragment 1 arrives, filling the hole.
        fs.insert(make_sample("B", 1, 5), 1, 5).unwrap();
        assert!(fs.missing_holes().is_empty());
        // Hole (2, 3) opens: it must be reported.
        let s5 = make_sample("F", 0, 6);
        let mut fs2 =
            FragmentedSample::from_first_fragment(s5, 0, 6, MAX_FRAGMENTS_DEFAULT).unwrap();
        fs2.insert(make_sample("G", 1, 6), 1, 6).unwrap();
        fs2.insert(make_sample("H", 4, 6), 4, 6).unwrap();
        assert_eq!(fs2.new_holes(), vec![(Some(2), Some(3))]);
        // Reshaping: fragment 2 arrives, the hole shrinks to (3, 3). The new
        // bounds are within the recorded (2, 3), but `insert` pruned that
        // range (it contains fragment 2), so the shrunk hole must be
        // re-reported.
        fs2.insert(make_sample("I", 2, 6), 2, 6).unwrap();
        assert_eq!(fs2.new_holes(), vec![(Some(3), Some(3))]);
    }

    #[test]
    fn missing_tail_only() {
        // Sequential prefix 0, 1, 2 of 5: no hole, tail at 3..
        let s0 = make_sample("A", 0, 5);
        let s1 = make_sample("B", 1, 5);
        let s2 = make_sample("C", 2, 5);
        let mut fs =
            FragmentedSample::from_first_fragment(s0, 0, 5, MAX_FRAGMENTS_DEFAULT).unwrap();
        fs.insert(s1, 1, 5).unwrap();
        fs.insert(s2, 2, 5).unwrap();
        assert!(fs.missing_holes().is_empty());
        assert_eq!(fs.missing_tail(), Some((Some(3), None)));
    }

    #[test]
    fn single_promotion_count_mismatch() {
        let s = make_sample("A", 0, 1);
        let mut fs = FragmentedSample::single(s);
        let s3 = make_sample("B", 0, 3);
        assert!(matches!(
            fs.insert(s3, 0, 3),
            Err(FragInsertError::CountMismatch {
                expected: 1,
                got: 3
            })
        ));
    }

    #[test]
    fn insert_fragment_duplicate_overwrites() {
        let s0 = make_sample("A", 0, 3);
        let s0_dup = make_sample("X", 0, 3);
        let s1 = make_sample("B", 1, 3);
        let s2 = make_sample("C", 2, 3);

        let mut fs =
            FragmentedSample::from_first_fragment(s0, 0, 3, MAX_FRAGMENTS_DEFAULT).unwrap();
        fs.insert(s0_dup, 0, 3).unwrap(); // duplicate, overwrites
        fs.insert(s1, 1, 3).unwrap();
        fs.insert(s2, 2, 3).unwrap();

        let sample = fs.into_sample().unwrap();
        assert_eq!(sample.payload().try_to_string().unwrap().as_ref(), "XBC");
    }

    #[test]
    fn insert_single_duplicate_overwrites() {
        let s = make_sample("A", 0, 1);
        let mut fs = FragmentedSample::single(s);
        fs.insert(make_sample("X", 0, 1), 0, 1).unwrap();
        assert_eq!(
            fs.into_sample()
                .unwrap()
                .payload()
                .try_to_string()
                .unwrap()
                .as_ref(),
            "X"
        );
    }

    #[test]
    fn fragment_count_exact_division() {
        assert_eq!(fragment_count(12, 4).unwrap(), 3);
    }

    #[test]
    fn fragment_count_leaves_remainder() {
        assert_eq!(fragment_count(13, 4).unwrap(), 4);
    }

    #[test]
    fn fragment_count_fits_even_beyond_default_cap() {
        // The subscriber-side cap is not enforced here: 4097 fragments exceed
        // MAX_FRAGMENTS_DEFAULT but are representable on the wire.
        assert_eq!(
            fragment_count(MAX_FRAGMENTS_DEFAULT as usize + 1, 1).unwrap(),
            MAX_FRAGMENTS_DEFAULT + 1
        );
    }

    #[test]
    fn fragment_count_max_u32() {
        assert_eq!(fragment_count(u32::MAX as usize, 1).unwrap(), u32::MAX);
    }

    #[test]
    fn fragment_count_overflow() {
        assert!(fragment_count(u32::MAX as usize + 1, 1).is_err());
        assert!(fragment_count(usize::MAX / 2, 1).is_err());
    }
}
