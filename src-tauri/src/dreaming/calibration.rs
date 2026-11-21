//! Similarity calibration. cloud-1's thresholds were written for Nomic Embed
//! V2 scores; Garden's index may hold any model (default_x1: bge-small,
//! whose random pairs score a median 0.53 where Nomic's score 0.25). Each
//! cycle samples random block pairs from the active index and maps a raw
//! score to the Nomic score at the same quantile, so cloud-1's constants keep
//! their meaning under any embedder.
//!
//! The reference table was measured 2026-10-02 on 3,000 of default_x1's
//! blocks embedded with both models (749,750 seed-to-block pairs; seeds with
//! the "clustering: " prefix and blocks with "search_document: ", the way
//! cloud-1 queried Qdrant). Above the 0.99995 quantile the table is too
//! sparse to trust, so the top is anchored at identity: a raw 1.0 (the same
//! text) is 1.0, linear in between.

use super::rng::SplitMix64;

/// Block-to-block pairs: (quantile, Nomic V2 similarity), from the
/// 2026-10-02 measurement.
const BLOCK_TABLE: &[(f64, f64)] = &[
    (0.0, -0.0857),
    (0.01, 0.0548),
    (0.02, 0.0737),
    (0.03, 0.0862),
    (0.04, 0.0959),
    (0.05, 0.1037),
    (0.06, 0.1107),
    (0.07, 0.1169),
    (0.08, 0.1225),
    (0.09, 0.1277),
    (0.1, 0.1325),
    (0.11, 0.137),
    (0.12, 0.1413),
    (0.13, 0.1454),
    (0.14, 0.1494),
    (0.15, 0.1531),
    (0.16, 0.1568),
    (0.17, 0.1603),
    (0.18, 0.1638),
    (0.19, 0.1671),
    (0.2, 0.1704),
    (0.21, 0.1736),
    (0.22, 0.1768),
    (0.23, 0.1798),
    (0.24, 0.1828),
    (0.25, 0.1857),
    (0.26, 0.1887),
    (0.27, 0.1917),
    (0.28, 0.1945),
    (0.29, 0.1974),
    (0.3, 0.2001),
    (0.31, 0.2029),
    (0.32, 0.2056),
    (0.33, 0.2084),
    (0.34, 0.2111),
    (0.35, 0.2138),
    (0.36, 0.2166),
    (0.37, 0.2193),
    (0.38, 0.2219),
    (0.39, 0.2246),
    (0.4, 0.2272),
    (0.41, 0.2299),
    (0.42, 0.2325),
    (0.43, 0.2351),
    (0.44, 0.2377),
    (0.45, 0.2404),
    (0.46, 0.2431),
    (0.47, 0.2457),
    (0.48, 0.2483),
    (0.49, 0.2508),
    (0.5, 0.2535),
    (0.51, 0.2561),
    (0.52, 0.2586),
    (0.53, 0.2612),
    (0.54, 0.2638),
    (0.55, 0.2664),
    (0.56, 0.269),
    (0.57, 0.2717),
    (0.58, 0.2743),
    (0.59, 0.2769),
    (0.6, 0.2796),
    (0.61, 0.2822),
    (0.62, 0.2849),
    (0.63, 0.2876),
    (0.64, 0.2903),
    (0.65, 0.293),
    (0.66, 0.2957),
    (0.67, 0.2984),
    (0.68, 0.3013),
    (0.69, 0.3041),
    (0.7, 0.307),
    (0.71, 0.3098),
    (0.72, 0.3127),
    (0.73, 0.3156),
    (0.74, 0.3186),
    (0.75, 0.3216),
    (0.76, 0.3246),
    (0.77, 0.3278),
    (0.78, 0.3309),
    (0.79, 0.3341),
    (0.8, 0.3375),
    (0.81, 0.3409),
    (0.82, 0.3445),
    (0.83, 0.3481),
    (0.84, 0.3517),
    (0.85, 0.3556),
    (0.86, 0.3596),
    (0.87, 0.3638),
    (0.88, 0.3682),
    (0.89, 0.3728),
    (0.9, 0.3777),
    (0.91, 0.3829),
    (0.92, 0.3887),
    (0.93, 0.3949),
    (0.94, 0.4018),
    (0.95, 0.4096),
    (0.96, 0.419),
    (0.97, 0.4302),
    (0.98, 0.4454),
    (0.99, 0.4702),
    (0.991, 0.4737),
    (0.992, 0.4774),
    (0.993, 0.4819),
    (0.994, 0.487),
    (0.995, 0.493),
    (0.996, 0.5007),
    (0.997, 0.5103),
    (0.998, 0.5235),
    (0.999, 0.5458),
    (0.9991, 0.55),
    (0.9992, 0.5543),
    (0.9993, 0.5582),
    (0.9994, 0.5641),
    (0.9995, 0.5696),
    (0.9996, 0.5788),
    (0.9997, 0.5901),
    (0.9998, 0.6052),
    (0.9999, 0.6305),
    (0.99995, 0.6665),
];

/// Document centroids (mean of a document's first five block vectors, as
/// cloud-1's isomorphism distance used, both sides "search_document: "):
/// (quantile, Nomic V2 cosine), measured 2026-10-02 on 320 default_x1
/// documents (51,040 pairs). Centroids sit much higher than single blocks
/// (Nomic median 0.68 against 0.25), so they need their own reference.
const CENTROID_TABLE: &[(f64, f64)] = &[
    (0.0, 0.248),
    (0.01, 0.4384),
    (0.02, 0.4668),
    (0.03, 0.4855),
    (0.04, 0.4992),
    (0.05, 0.5114),
    (0.06, 0.5206),
    (0.07, 0.5291),
    (0.08, 0.5374),
    (0.09, 0.5446),
    (0.1, 0.5514),
    (0.11, 0.558),
    (0.12, 0.5638),
    (0.13, 0.5695),
    (0.14, 0.5746),
    (0.15, 0.5795),
    (0.16, 0.5841),
    (0.17, 0.5883),
    (0.18, 0.5926),
    (0.19, 0.5969),
    (0.2, 0.6005),
    (0.21, 0.6043),
    (0.22, 0.6076),
    (0.23, 0.6109),
    (0.24, 0.6142),
    (0.25, 0.6174),
    (0.26, 0.6205),
    (0.27, 0.6233),
    (0.28, 0.626),
    (0.29, 0.6288),
    (0.3, 0.6314),
    (0.31, 0.634),
    (0.32, 0.6365),
    (0.33, 0.639),
    (0.34, 0.6416),
    (0.35, 0.644),
    (0.36, 0.6465),
    (0.37, 0.6489),
    (0.38, 0.6513),
    (0.39, 0.6535),
    (0.4, 0.6558),
    (0.41, 0.6579),
    (0.42, 0.6601),
    (0.43, 0.6621),
    (0.44, 0.6642),
    (0.45, 0.6663),
    (0.46, 0.6683),
    (0.47, 0.6706),
    (0.48, 0.6727),
    (0.49, 0.6746),
    (0.5, 0.6766),
    (0.51, 0.6786),
    (0.52, 0.6806),
    (0.53, 0.6826),
    (0.54, 0.6845),
    (0.55, 0.6865),
    (0.56, 0.6884),
    (0.57, 0.6903),
    (0.58, 0.6922),
    (0.59, 0.6942),
    (0.6, 0.6961),
    (0.61, 0.698),
    (0.62, 0.7),
    (0.63, 0.7018),
    (0.64, 0.7037),
    (0.65, 0.7055),
    (0.66, 0.7073),
    (0.67, 0.7091),
    (0.68, 0.711),
    (0.69, 0.7131),
    (0.7, 0.7149),
    (0.71, 0.7169),
    (0.72, 0.7189),
    (0.73, 0.721),
    (0.74, 0.7231),
    (0.75, 0.7252),
    (0.76, 0.7275),
    (0.77, 0.7296),
    (0.78, 0.7316),
    (0.79, 0.7339),
    (0.8, 0.7359),
    (0.81, 0.7382),
    (0.82, 0.7405),
    (0.83, 0.7429),
    (0.84, 0.7454),
    (0.85, 0.7479),
    (0.86, 0.7506),
    (0.87, 0.7533),
    (0.88, 0.7561),
    (0.89, 0.7591),
    (0.9, 0.7623),
    (0.91, 0.7655),
    (0.92, 0.7691),
    (0.93, 0.7735),
    (0.94, 0.778),
    (0.95, 0.7838),
    (0.96, 0.7906),
    (0.97, 0.8001),
    (0.98, 0.8148),
    (0.99, 0.844),
    (1.0, 1.0),
];

#[derive(Debug, Clone, Copy)]
pub(crate) struct Reference {
    table: &'static [(f64, f64)],
    /// Above this quantile the reference is too sparse; the top is linear
    /// to identity (a raw 1.0 is 1.0).
    reliable_to: f64,
}

pub(crate) const BLOCK_PAIRS: Reference = Reference { table: BLOCK_TABLE, reliable_to: 0.99995 };
pub(crate) const CENTROIDS: Reference = Reference { table: CENTROID_TABLE, reliable_to: 0.99 };

impl Reference {
    fn at(&self, quantile: f64) -> f64 {
        let t = self.table;
        let index = t.partition_point(|(q, _)| *q < quantile);
        if index == 0 {
            return t[0].1;
        }
        if index >= t.len() {
            return t[t.len() - 1].1;
        }
        let (q0, s0) = t[index - 1];
        let (q1, s1) = t[index];
        if q1 <= q0 {
            return s1;
        }
        s0 + (quantile - q0) / (q1 - q0) * (s1 - s0)
    }
}

/// Seeds × targets random pairs; enough to resolve the 0.99995 quantile
/// with ~40 pairs above it.
const SAMPLE_SEEDS: usize = 2000;
const SAMPLE_TARGETS: usize = 400;

#[derive(Debug, Clone)]
pub(crate) struct Calibration {
    /// Sorted raw scores of the sampled pairs; empty means identity.
    sample: Vec<f32>,
    /// The raw score at the last reliable reference quantile.
    top_raw: f64,
    reference: Reference,
}

impl Calibration {
    pub(crate) fn identity() -> Self {
        Self { sample: Vec::new(), top_raw: 1.0, reference: BLOCK_PAIRS }
    }

    /// Calibrate against `reference` from a sample of raw scores.
    pub(crate) fn from_sample(mut sample: Vec<f32>, reference: Reference) -> Self {
        if sample.len() < 100 {
            return Self::identity();
        }
        sample.sort_by(|x, y| x.total_cmp(y));
        // Clamped below 1: a graph with many exact duplicates would otherwise
        // put the anchor at 1.0 (or a float overshoot), and copies would
        // calibrate as strangers.
        let top_raw = f64::from(sample[((sample.len() - 1) as f64 * reference.reliable_to) as usize]).min(0.999);
        Self { sample, top_raw, reference }
    }

    /// Calibration for document centroids: all pairs among up to 400 of
    /// `centroids` (unit length).
    pub(crate) fn measure_centroids(centroids: &[Vec<f32>], seed: u64) -> Self {
        let mut chosen: Vec<&Vec<f32>> = centroids.iter().collect();
        SplitMix64::new(seed ^ 0xce17).shuffle(&mut chosen);
        chosen.truncate(400);
        let mut sample = Vec::new();
        for i in 0..chosen.len() {
            for j in i + 1..chosen.len() {
                sample.push(dot(chosen[i], chosen[j]));
            }
        }
        Self::from_sample(sample, CENTROIDS)
    }

    /// Sample random pairs among `vectors` (unit length, equal dimensions).
    pub(crate) fn measure(vectors: &[&[f32]], seed: u64) -> Self {
        if vectors.len() < 50 {
            return Self::identity();
        }
        let mut rng = SplitMix64::new(seed);
        let mut sample = Vec::with_capacity(SAMPLE_SEEDS * SAMPLE_TARGETS);
        for _ in 0..SAMPLE_SEEDS {
            let a = rng.below(vectors.len());
            for _ in 0..SAMPLE_TARGETS {
                let b = rng.below(vectors.len());
                if a != b {
                    sample.push(dot(vectors[a], vectors[b]));
                }
            }
        }
        Self::from_sample(sample, BLOCK_PAIRS)
    }

    /// The raw score expressed on cloud-1's Nomic scale.
    pub(crate) fn calibrate(&self, raw: f32) -> f64 {
        let raw = f64::from(raw);
        if self.sample.is_empty() {
            return raw;
        }
        let top_reference = self.reference.at(self.reference.reliable_to);
        if raw >= self.top_raw {
            if self.top_raw >= 1.0 {
                return 1.0;
            }
            let t = ((raw - self.top_raw) / (1.0 - self.top_raw)).clamp(0.0, 1.0);
            return top_reference + t * (1.0 - top_reference);
        }
        let below = self.sample.partition_point(|s| f64::from(*s) < raw);
        let quantile = below as f64 / self.sample.len() as f64;
        self.reference.at(quantile)
    }

    pub(crate) fn summary(&self) -> serde_json::Value {
        if self.sample.is_empty() {
            return serde_json::json!({ "method": "identity" });
        }
        let at = |q: f64| self.sample[((self.sample.len() - 1) as f64 * q) as usize];
        serde_json::json!({
            "method": "quantile-matched to a Nomic V2 reference measured 2026-10-02",
            "pairs": self.sample.len(),
            "rawMedian": at(0.5),
            "rawP99": at(0.99),
            "rawAtTopReference": self.top_raw,
        })
    }
}

pub(crate) fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_is_monotone() {
        for pair in BLOCK_TABLE.windows(2).chain(CENTROID_TABLE.windows(2)) {
            assert!(pair[0].0 < pair[1].0 && pair[0].1 <= pair[1].1, "{pair:?}");
        }
    }

    #[test]
    fn a_reference_shaped_sample_maps_onto_itself() {
        // Raw scores distributed exactly as the reference: calibration is
        // (close to) the identity across the body of the distribution.
        let mut sample = Vec::new();
        for i in 0..100_000 {
            sample.push(BLOCK_PAIRS.at(i as f64 / 100_000.0) as f32);
        }
        let c = Calibration::from_sample(sample, BLOCK_PAIRS);
        for raw in [0.1_f32, 0.25, 0.3, 0.45, 0.5] {
            assert!((c.calibrate(raw) - f64::from(raw)).abs() < 0.01, "{raw} -> {}", c.calibrate(raw));
        }
        assert_eq!(c.calibrate(1.0), 1.0);
    }

    #[test]
    fn bge_scale_lands_where_the_measurement_put_it() {
        // default_x1's measured bge-small quantiles (calibration2.json) as
        // the sample: cloud's band edges must come back near the values the
        // direct two-model measurement found (0.563 ~ 0.3, 0.735 ~ 0.5).
        let bge: &[(f64, f64)] = &[
            (0.0, 0.25), (0.05, 0.414), (0.25, 0.48), (0.5, 0.529), (0.6756, 0.5628),
            (0.75, 0.579), (0.95, 0.655), (0.99, 0.71), (0.9959, 0.7352), (0.9998, 0.799),
            (0.99995, 0.8414), (1.0, 1.0),
        ];
        let interp = |q: f64| {
            let i = bge.partition_point(|(x, _)| *x < q).clamp(1, bge.len() - 1);
            let ((q0, s0), (q1, s1)) = (bge[i - 1], bge[i]);
            s0 + (q - q0) / (q1 - q0) * (s1 - s0)
        };
        let mut sample: Vec<f32> = (0..200_000).map(|i| interp(i as f64 / 200_000.0) as f32).collect();
        let c = Calibration::from_sample(sample, BLOCK_PAIRS);
        assert!((c.calibrate(0.5628) - 0.3).abs() < 0.02, "{}", c.calibrate(0.5628));
        assert!((c.calibrate(0.7352) - 0.5).abs() < 0.02, "{}", c.calibrate(0.7352));
        // Exact copies sit above the near-copy ceiling; a merely close pair does not.
        assert!(c.calibrate(0.99) > 0.92);
        assert!(c.calibrate(0.80) < 0.92);
    }

    #[test]
    fn bge_centroids_land_on_the_nomic_centroid_scale() {
        // default_x1's measured bge centroid quantiles (centroids.json):
        // the bge median 0.777 must come back near Nomic's 0.677.
        let bge: &[(f64, f64)] = &[(0.0, 0.5062), (0.1, 0.692), (0.5, 0.7774), (0.9, 0.8408), (1.0, 1.0)];
        let interp = |q: f64| {
            let i = bge.partition_point(|(x, _)| *x < q).clamp(1, bge.len() - 1);
            let ((q0, s0), (q1, s1)) = (bge[i - 1], bge[i]);
            s0 + (q - q0) / (q1 - q0) * (s1 - s0)
        };
        let sample: Vec<f32> = (0..50_000).map(|i| interp(i as f64 / 50_000.0) as f32).collect();
        let c = Calibration::from_sample(sample, CENTROIDS);
        assert!((c.calibrate(0.7774) - 0.6766).abs() < 0.01, "{}", c.calibrate(0.7774));
        assert!((c.calibrate(0.692) - 0.5514).abs() < 0.01, "{}", c.calibrate(0.692));
    }

    #[test]
    fn many_duplicates_still_calibrate_copies_as_copies() {
        // A sample whose top tail is all exact copies (raw 1.0).
        let mut sample: Vec<f32> = (0..100_000).map(|i| BLOCK_PAIRS.at(i as f64 / 100_000.0) as f32).collect();
        for s in sample.iter_mut().rev().take(100) {
            *s = 1.000_001;
        }
        let c = Calibration::from_sample(sample, BLOCK_PAIRS);
        assert!(c.calibrate(1.0) > 0.92, "{}", c.calibrate(1.0));
        assert!(c.calibrate(0.9999) > 0.92);
    }
}
