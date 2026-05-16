use std::cmp::PartialEq;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::ops::{Add, Deref, Div, Sub};
use std::sync::Arc;

use ahash::{AHasher, HashSet};
use anyhow::{Result, ensure};
use blake3::Hasher as Blake3Hasher;
use ndarray::{Array1, LinalgScalar, Zip};
use ndarray_stats::DeviationExt;
use num_traits::Zero;
use revision::{DeserializeRevisioned, SerializeRevisioned, revisioned};
use rust_decimal::prelude::FromPrimitive;
use storekey::{BorrowDecode, BorrowReader, DecodeError, Encode, EncodeError, Writer};

use crate::catalog::{Distance, VectorType};
use crate::err::Error;
use crate::fnc::util::math::ToFloat;
use crate::kvs::KVValue;
use crate::val::{Number, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum Vector {
	F64(Array1<f64>),
	F32(Array1<f32>),
	I64(Array1<i64>),
	I32(Array1<i32>),
	I16(Array1<i16>),
}

#[revisioned(revision = 1)]
#[derive(Clone, Debug, PartialEq)]
pub enum SerializedVector {
	F64(Vec<f64>),
	F32(Vec<f32>),
	I64(Vec<i64>),
	I32(Vec<i32>),
	I16(Vec<i16>),
}

impl KVValue for SerializedVector {
	#[inline]
	fn kv_encode_value(&self) -> Result<Vec<u8>> {
		let mut val = Vec::new();
		SerializeRevisioned::serialize_revisioned(self, &mut val)?;
		Ok(val)
	}

	#[inline]
	fn kv_decode_value(val: Vec<u8>) -> Result<Self> {
		Ok(DeserializeRevisioned::deserialize_revisioned(&mut val.as_slice())?)
	}
}

impl<F> Encode<F> for SerializedVector {
	#[inline]
	fn encode<W: Write>(&self, w: &mut Writer<W>) -> std::result::Result<(), EncodeError> {
		// Capacity hint: payload bytes + small overhead for revision header/length.
		let cap = match self {
			SerializedVector::F64(v) => v.len() * 8 + 16,
			SerializedVector::F32(v) => v.len() * 4 + 16,
			SerializedVector::I64(v) => v.len() * 8 + 16,
			SerializedVector::I32(v) => v.len() * 4 + 16,
			SerializedVector::I16(v) => v.len() * 2 + 16,
		};
		let mut buf = Vec::with_capacity(cap);
		SerializeRevisioned::serialize_revisioned(self, &mut buf).map_err(EncodeError::custom)?;
		w.write_slice(&buf)?;
		Ok(())
	}
}

impl<'de, F> BorrowDecode<'de, F> for SerializedVector {
	fn borrow_decode(r: &mut BorrowReader<'de>) -> std::result::Result<Self, DecodeError> {
		let slice = r.read_cow()?;
		let bytes: &[u8] = slice.as_ref();
		let mut reader = bytes;
		DeserializeRevisioned::deserialize_revisioned(&mut reader).map_err(DecodeError::custom)
	}
}

impl From<&Vector> for SerializedVector {
	fn from(value: &Vector) -> Self {
		match value {
			Vector::F64(v) => Self::F64(v.to_vec()),
			Vector::F32(v) => Self::F32(v.to_vec()),
			Vector::I64(v) => Self::I64(v.to_vec()),
			Vector::I32(v) => Self::I32(v.to_vec()),
			Vector::I16(v) => Self::I16(v.to_vec()),
		}
	}
}

impl From<SerializedVector> for Vector {
	fn from(value: SerializedVector) -> Self {
		match value {
			SerializedVector::F64(v) => Self::F64(Array1::from_vec(v)),
			SerializedVector::F32(v) => Self::F32(Array1::from_vec(v)),
			SerializedVector::I64(v) => Self::I64(Array1::from_vec(v)),
			SerializedVector::I32(v) => Self::I32(Array1::from_vec(v)),
			SerializedVector::I16(v) => Self::I16(Array1::from_vec(v)),
		}
	}
}

impl SerializedVector {
	pub(super) fn try_from_value(t: VectorType, d: usize, v: Value) -> Result<Self> {
		let res = match t {
			VectorType::F64 => {
				let mut vec = Vec::with_capacity(d);
				Self::check_vector_value(v, &mut vec)?;
				Self::F64(vec)
			}
			VectorType::F32 => {
				let mut vec = Vec::with_capacity(d);
				Self::check_vector_value(v, &mut vec)?;
				Self::F32(vec)
			}
			VectorType::I64 => {
				let mut vec = Vec::with_capacity(d);
				Self::check_vector_value(v, &mut vec)?;
				Self::I64(vec)
			}
			VectorType::I32 => {
				let mut vec = Vec::with_capacity(d);
				Self::check_vector_value(v, &mut vec)?;
				Self::I32(vec)
			}
			VectorType::I16 => {
				let mut vec = Vec::with_capacity(d);
				Self::check_vector_value(v, &mut vec)?;
				Self::I16(vec)
			}
		};
		Ok(res)
	}

	fn check_vector_value<T>(value: Value, vec: &mut Vec<T>) -> Result<()>
	where
		T: TryFrom<Number, Error = Error>,
	{
		match value {
			Value::Array(a) => {
				for v in a.0 {
					Self::check_vector_value(v, vec)?;
				}
				Ok(())
			}
			Value::Number(n) => {
				vec.push(n.try_into()?);
				Ok(())
			}
			_ => Err(anyhow::Error::new(Error::InvalidVectorValue(value.to_raw_string()))),
		}
	}

	pub(super) fn dimension(&self) -> usize {
		match self {
			Self::F64(v) => v.len(),
			Self::F32(v) => v.len(),
			Self::I64(v) => v.len(),
			Self::I32(v) => v.len(),
			Self::I16(v) => v.len(),
		}
	}

	/// Computes a BLAKE3 hash of the vector's bytes.
	///
	/// This is used for deduplicating vectors in the HNSW index when `HASHED_VECTOR` is enabled.
	/// The hash is calculated by iterating over the vector elements and updating the hasher
	/// with their little-endian byte representation.
	pub(crate) fn compute_hash(&self) -> [u8; 32] {
		let mut hasher = Blake3Hasher::new();
		match self {
			Self::F64(v) => {
				for &val in v {
					hasher.update(&val.to_le_bytes());
				}
			}
			Self::F32(v) => {
				for &val in v {
					hasher.update(&val.to_le_bytes());
				}
			}
			Self::I64(v) => {
				for &val in v {
					hasher.update(&val.to_le_bytes());
				}
			}
			Self::I32(v) => {
				for &val in v {
					hasher.update(&val.to_le_bytes());
				}
			}
			Self::I16(v) => {
				for &val in v {
					hasher.update(&val.to_le_bytes());
				}
			}
		}
		*hasher.finalize().as_bytes()
	}
}

impl Vector {
	#[inline]
	fn chebyshev<T>(a: &Array1<T>, b: &Array1<T>) -> f64
	where
		T: ToFloat,
	{
		a.iter()
			.zip(b.iter())
			.map(|(a, b)| (a.to_float() - b.to_float()).abs())
			.fold(0.0_f64, f64::max)
	}

	#[cfg(not(feature = "vector-hpc"))]
	fn chebyshev_distance(&self, other: &Self) -> f64 {
		match (self, other) {
			(Self::F64(a), Self::F64(b)) => a.linf_dist(b).unwrap_or(f64::INFINITY),
			(Self::F32(a), Self::F32(b)) => {
				a.linf_dist(b).map(|r| r as f64).unwrap_or(f64::INFINITY)
			}
			(Self::I64(a), Self::I64(b)) => {
				a.linf_dist(b).map(|r| r as f64).unwrap_or(f64::INFINITY)
			}
			(Self::I32(a), Self::I32(b)) => {
				a.linf_dist(b).map(|r| r as f64).unwrap_or(f64::INFINITY)
			}
			(Self::I16(a), Self::I16(b)) => Self::chebyshev(a, b),
			_ => f64::NAN,
		}
	}

	#[cfg(feature = "vector-hpc")]
	fn chebyshev_distance(&self, other: &Self) -> f64 {
		match (self, other) {
			(Self::F64(a), Self::F64(b)) => match (a.as_slice(), b.as_slice()) {
				(Some(a_s), Some(b_s)) => Self::chebyshev_distance_f64_simd(a_s, b_s),
				_ => {
					let a_v: Vec<f64> = a.iter().copied().collect();
					let b_v: Vec<f64> = b.iter().copied().collect();
					Self::chebyshev_distance_f64_simd(&a_v, &b_v)
				}
			},
			// f32 → widen to f64 for SIMD precision.
			(Self::F32(a), Self::F32(b)) => {
				let a_v: Vec<f64> = a.iter().map(|&x| x as f64).collect();
				let b_v: Vec<f64> = b.iter().map(|&x| x as f64).collect();
				Self::chebyshev_distance_f64_simd(&a_v, &b_v)
			}
			(Self::I64(a), Self::I64(b)) => {
				a.linf_dist(b).map(|r| r as f64).unwrap_or(f64::INFINITY)
			}
			(Self::I32(a), Self::I32(b)) => {
				a.linf_dist(b).map(|r| r as f64).unwrap_or(f64::INFINITY)
			}
			(Self::I16(a), Self::I16(b)) => Self::chebyshev(a, b),
			_ => f64::NAN,
		}
	}

	#[cfg(not(feature = "vector-hpc"))]
	#[inline]
	fn cosine_distance_f64(a: &Array1<f64>, b: &Array1<f64>) -> f64 {
		let dot_product = a.dot(b);
		let norm_a = (a * a).sum().sqrt();
		let norm_b = (b * b).sum().sqrt();
		1.0 - dot_product / (norm_a * norm_b)
	}

	#[cfg(feature = "vector-hpc")]
	#[inline]
	fn cosine_distance_f64(a: &Array1<f64>, b: &Array1<f64>) -> f64 {
		// SIMD dispatch lineage:
		//   ndarray::hpc::heel_f64x8::cosine_f64_simd  (thin kernel)
		//   └── built on ndarray::simd::F64x8  (8-lane polyfill type)
		//       └── dispatches via static LazyLock<Tier> at simd.rs:92,
		//           which runs CPU feature detection ONCE at startup.
		//           Subsequent calls are hardware-agnostic: the same Rust
		//           surface compiles for AVX-512, AVX2, NEON, or scalar
		//           fallback, dispatched through the cached Tier.
		//
		// SAFETY: as_slice returns Some when storage is contiguous, which is
		// the case for vectors constructed via Array1::from_vec (SurrealDB's
		// standard path). For non-contiguous views (rare in this codebase),
		// fall back to a one-shot to_vec().
		match (a.as_slice(), b.as_slice()) {
			(Some(a_s), Some(b_s)) => {
				// Fast path — contiguous storage, zero-copy.
				let similarity =
					ndarray::hpc::heel_f64x8::cosine_f64_simd(a_s, b_s);
				1.0 - similarity
			}
			_ => {
				// Fallback — non-contiguous; clone-to-owned first.
				let a_v: Vec<f64> = a.iter().copied().collect();
				let b_v: Vec<f64> = b.iter().copied().collect();
				let similarity =
					ndarray::hpc::heel_f64x8::cosine_f64_simd(&a_v, &b_v);
				1.0 - similarity
			}
		}
	}

	#[cfg(not(feature = "vector-hpc"))]
	#[inline]
	fn cosine_distance_f32(a: &Array1<f32>, b: &Array1<f32>) -> f64 {
		let dot_product = a.dot(b) as f64;
		let norm_a = ((a * a).sum() as f64).sqrt();
		let norm_b = ((b * b).sum() as f64).sqrt();
		1.0 - dot_product / (norm_a * norm_b)
	}

	#[cfg(feature = "vector-hpc")]
	#[inline]
	fn cosine_distance_f32(a: &Array1<f32>, b: &Array1<f32>) -> f64 {
		// SAFETY: as_slice returns Some when storage is contiguous, which is
		// the case for vectors constructed via Array1::from_vec (SurrealDB's
		// standard path). For non-contiguous views (rare in this codebase),
		// fall back to a one-shot to_vec().
		match (a.as_slice(), b.as_slice()) {
			(Some(a_s), Some(b_s)) => {
				// Fast path — contiguous storage, zero-copy.
				let similarity =
					ndarray::hpc::heel_f64x8::cosine_f32_to_f64_simd(a_s, b_s);
				1.0 - similarity
			}
			_ => {
				// Fallback — non-contiguous; clone-to-owned first.
				let a_v: Vec<f32> = a.iter().copied().collect();
				let b_v: Vec<f32> = b.iter().copied().collect();
				let similarity =
					ndarray::hpc::heel_f64x8::cosine_f32_to_f64_simd(&a_v, &b_v);
				1.0 - similarity
			}
		}
	}

	#[inline]
	fn cosine_dist<T>(a: &Array1<T>, b: &Array1<T>) -> f64
	where
		T: ToFloat + LinalgScalar,
	{
		let dot_product = a.dot(b).to_float();
		let norm_a = a.mapv(|x| x.to_float() * x.to_float()).sum().sqrt();
		let norm_b = b.mapv(|x| x.to_float() * x.to_float()).sum().sqrt();
		1.0 - dot_product / (norm_a * norm_b)
	}

	fn cosine_distance(&self, other: &Self) -> f64 {
		match (self, other) {
			(Self::F64(a), Self::F64(b)) => Self::cosine_distance_f64(a, b),
			(Self::F32(a), Self::F32(b)) => Self::cosine_distance_f32(a, b),
			(Self::I64(a), Self::I64(b)) => Self::cosine_dist(a, b),
			(Self::I32(a), Self::I32(b)) => Self::cosine_dist(a, b),
			(Self::I16(a), Self::I16(b)) => Self::cosine_dist(a, b),
			_ => f64::INFINITY,
		}
	}

	#[inline]
	fn euclidean<T>(a: &Array1<T>, b: &Array1<T>) -> f64
	where
		T: ToFloat,
	{
		Zip::from(a).and(b).map_collect(|x, y| (x.to_float() - y.to_float()).powi(2)).sum().sqrt()
	}

	/// SIMD L2 distance written directly against the polyfill type
	/// `ndarray::simd::F64x8`. CPU detection is cached once in
	/// `LazyLock<Tier>` (simd.rs:92); subsequent calls dispatch via the
	/// cached tier (AVX-512 / AVX2 / NEON / scalar fallback). Hardware-
	/// agnostic — same Rust surface compiles on every target.
	#[cfg(feature = "vector-hpc")]
	#[inline]
	fn euclidean_distance_f64_simd(a: &[f64], b: &[f64]) -> f64 {
		use ndarray::simd::F64x8;
		let n = a.len().min(b.len());
		let chunks = n / 8;
		let remainder = n % 8;

		let mut sum_sq = F64x8::splat(0.0);
		for i in 0..chunks {
			let va = F64x8::from_slice(&a[i * 8..i * 8 + 8]);
			let vb = F64x8::from_slice(&b[i * 8..i * 8 + 8]);
			let diff = va - vb;
			// sum_sq += diff * diff (fused multiply-add).
			sum_sq = diff.mul_add(diff, sum_sq);
		}

		let mut acc = sum_sq.reduce_sum();
		let offset = chunks * 8;
		for i in 0..remainder {
			let d = a[offset + i] - b[offset + i];
			acc += d * d;
		}
		acc.sqrt()
	}

	/// SIMD L1 (Manhattan) distance written directly against `ndarray::simd::F64x8`.
	/// CPU detection is cached once in `LazyLock<Tier>` (simd.rs:92); subsequent
	/// calls dispatch via the cached tier (AVX-512 / AVX2 / NEON / scalar fallback).
	#[cfg(feature = "vector-hpc")]
	#[inline]
	fn manhattan_distance_f64_simd(a: &[f64], b: &[f64]) -> f64 {
		use ndarray::simd::F64x8;
		let n = a.len().min(b.len());
		let chunks = n / 8;
		let remainder = n % 8;

		let mut acc = F64x8::splat(0.0);
		for i in 0..chunks {
			let va = F64x8::from_slice(&a[i * 8..i * 8 + 8]);
			let vb = F64x8::from_slice(&b[i * 8..i * 8 + 8]);
			acc = acc + (va - vb).abs();
		}
		let mut sum = acc.reduce_sum();
		let offset = chunks * 8;
		for i in 0..remainder {
			sum += (a[offset + i] - b[offset + i]).abs();
		}
		sum
	}

	/// SIMD L∞ (Chebyshev) distance written directly against `ndarray::simd::F64x8`.
	/// CPU detection is cached once in `LazyLock<Tier>` (simd.rs:92); subsequent
	/// calls dispatch via the cached tier (AVX-512 / AVX2 / NEON / scalar fallback).
	#[cfg(feature = "vector-hpc")]
	#[inline]
	fn chebyshev_distance_f64_simd(a: &[f64], b: &[f64]) -> f64 {
		use ndarray::simd::F64x8;
		let n = a.len().min(b.len());
		let chunks = n / 8;
		let remainder = n % 8;

		let mut acc = F64x8::splat(f64::NEG_INFINITY);
		for i in 0..chunks {
			let va = F64x8::from_slice(&a[i * 8..i * 8 + 8]);
			let vb = F64x8::from_slice(&b[i * 8..i * 8 + 8]);
			acc = acc.simd_max((va - vb).abs());
		}
		let mut m = acc.reduce_max();
		let offset = chunks * 8;
		for i in 0..remainder {
			m = m.max((a[offset + i] - b[offset + i]).abs());
		}
		// Empty inputs or all-NEG_INFINITY accumulator: distance is 0.
		if m.is_finite() { m } else { 0.0 }
	}

	/// SIMD Pearson correlation (centered cosine similarity) written against
	/// `ndarray::simd::F64x8`. Returns the correlation coefficient r
	/// (not the distance 1-r), matching the scalar `pearson` method contract.
	/// CPU detection is cached once in `LazyLock<Tier>` (simd.rs:92); subsequent
	/// calls dispatch via the cached tier (AVX-512 / AVX2 / NEON / scalar fallback).
	#[cfg(feature = "vector-hpc")]
	#[inline]
	fn pearson_similarity_f64_simd(a: &[f64], b: &[f64]) -> f64 {
		let n = a.len().min(b.len());
		if n == 0 {
			return 0.0; // match scalar: denominator==0 returns 0
		}
		let nf = n as f64;
		// Pass 1: compute means (scalar; this is O(n) not O(n/8) — acceptable
		// because Pass 2 dominates and the SIMD cosine kernel handles the bulk).
		let mean_a: f64 = a[..n].iter().sum::<f64>() / nf;
		let mean_b: f64 = b[..n].iter().sum::<f64>() / nf;
		// Pass 2: center the vectors.
		let ca: Vec<f64> = a[..n].iter().map(|x| x - mean_a).collect();
		let cb: Vec<f64> = b[..n].iter().map(|x| x - mean_b).collect();
		// Reuse the SIMD cosine kernel on centered vectors.
		// cosine_f64_simd returns dot/(|ca|*|cb|) which equals Pearson r.
		// When both norms are ~0 the kernel returns NaN; map to 0 to match scalar.
		let r = ndarray::hpc::heel_f64x8::cosine_f64_simd(&ca, &cb);
		if r.is_finite() { r } else { 0.0 }
	}

	#[cfg(not(feature = "vector-hpc"))]
	#[inline]
	fn euclidean_distance(&self, other: &Self) -> f64 {
		match (self, other) {
			(Self::F64(a), Self::F64(b)) => a.l2_dist(b).unwrap_or(f64::INFINITY),
			(Self::F32(a), Self::F32(b)) => a.l2_dist(b).unwrap_or(f64::INFINITY),
			(Self::I64(a), Self::I64(b)) => a.l2_dist(b).unwrap_or(f64::INFINITY),
			(Self::I32(a), Self::I32(b)) => a.l2_dist(b).unwrap_or(f64::INFINITY),
			(Self::I16(a), Self::I16(b)) => Self::euclidean(a, b),
			_ => f64::INFINITY,
		}
	}

	#[cfg(feature = "vector-hpc")]
	#[inline]
	fn euclidean_distance(&self, other: &Self) -> f64 {
		match (self, other) {
			(Self::F64(a), Self::F64(b)) => match (a.as_slice(), b.as_slice()) {
				(Some(a_s), Some(b_s)) => Self::euclidean_distance_f64_simd(a_s, b_s),
				_ => {
					let a_v: Vec<f64> = a.iter().copied().collect();
					let b_v: Vec<f64> = b.iter().copied().collect();
					Self::euclidean_distance_f64_simd(&a_v, &b_v)
				}
			},
			// f32 → widen to f64 for SIMD precision then take sqrt.
			(Self::F32(a), Self::F32(b)) => {
				let a_v: Vec<f64> = a.iter().map(|&x| x as f64).collect();
				let b_v: Vec<f64> = b.iter().map(|&x| x as f64).collect();
				Self::euclidean_distance_f64_simd(&a_v, &b_v)
			}
			(Self::I64(a), Self::I64(b)) => a.l2_dist(b).unwrap_or(f64::INFINITY),
			(Self::I32(a), Self::I32(b)) => a.l2_dist(b).unwrap_or(f64::INFINITY),
			(Self::I16(a), Self::I16(b)) => Self::euclidean(a, b),
			_ => f64::INFINITY,
		}
	}

	#[inline]
	fn hamming<T>(a: &Array1<T>, b: &Array1<T>) -> f64
	where
		T: PartialEq,
	{
		Zip::from(a).and(b).fold(0, |acc, a, b| {
			if a != b {
				acc + 1
			} else {
				acc
			}
		}) as f64
	}

	fn hamming_distance(&self, other: &Self) -> f64 {
		match (self, other) {
			(Self::F64(a), Self::F64(b)) => Self::hamming(a, b),
			(Self::F32(a), Self::F32(b)) => Self::hamming(a, b),
			(Self::I64(a), Self::I64(b)) => Self::hamming(a, b),
			(Self::I32(a), Self::I32(b)) => Self::hamming(a, b),
			(Self::I16(a), Self::I16(b)) => Self::hamming(a, b),
			_ => f64::INFINITY,
		}
	}

	#[inline]
	fn jaccard_f64(a: &Array1<f64>, b: &Array1<f64>) -> f64 {
		let mut union: HashSet<u64> = a.iter().map(|f| f.to_bits()).collect();
		let intersection_size = b.iter().fold(0, |acc, n| {
			if !union.insert(n.to_bits()) {
				acc + 1
			} else {
				acc
			}
		}) as f64;
		1.0 - intersection_size / union.len() as f64
	}

	#[inline]
	fn jaccard_f32(a: &Array1<f32>, b: &Array1<f32>) -> f64 {
		let mut union: HashSet<u32> = a.iter().map(|f| f.to_bits()).collect();
		let intersection_size = b.iter().fold(0, |acc, n| {
			if !union.insert(n.to_bits()) {
				acc + 1
			} else {
				acc
			}
		}) as f64;
		intersection_size / union.len() as f64
	}

	#[inline]
	fn jaccard_integers<T>(a: &Array1<T>, b: &Array1<T>) -> f64
	where
		T: Eq + Hash + Clone,
	{
		let mut union: HashSet<T> = a.iter().cloned().collect();
		let intersection_size = b.iter().cloned().fold(0, |acc, n| {
			if !union.insert(n) {
				acc + 1
			} else {
				acc
			}
		}) as f64;
		intersection_size / union.len() as f64
	}

	pub(super) fn jaccard_similarity(&self, other: &Self) -> f64 {
		match (self, other) {
			(Self::F64(a), Self::F64(b)) => Self::jaccard_f64(a, b),
			(Self::F32(a), Self::F32(b)) => Self::jaccard_f32(a, b),
			(Self::I64(a), Self::I64(b)) => Self::jaccard_integers(a, b),
			(Self::I32(a), Self::I32(b)) => Self::jaccard_integers(a, b),
			(Self::I16(a), Self::I16(b)) => Self::jaccard_integers(a, b),
			_ => f64::NAN,
		}
	}

	#[inline]
	fn manhattan<T>(a: &Array1<T>, b: &Array1<T>) -> f64
	where
		T: Sub<Output = T> + ToFloat + Copy,
	{
		a.iter().zip(b.iter()).map(|(&a, &b)| (a - b).to_float().abs()).sum()
	}

	#[cfg(not(feature = "vector-hpc"))]
	pub(super) fn manhattan_distance(&self, other: &Self) -> f64 {
		match (self, other) {
			(Self::F64(a), Self::F64(b)) => a.l1_dist(b).unwrap_or(f64::INFINITY),
			(Self::F32(a), Self::F32(b)) => a.l1_dist(b).map(|r| r as f64).unwrap_or(f64::INFINITY),
			(Self::I64(a), Self::I64(b)) => a.l1_dist(b).map(|r| r as f64).unwrap_or(f64::INFINITY),
			(Self::I32(a), Self::I32(b)) => a.l1_dist(b).map(|r| r as f64).unwrap_or(f64::INFINITY),
			(Self::I16(a), Self::I16(b)) => Self::manhattan(a, b),
			_ => f64::NAN,
		}
	}

	#[cfg(feature = "vector-hpc")]
	pub(super) fn manhattan_distance(&self, other: &Self) -> f64 {
		match (self, other) {
			(Self::F64(a), Self::F64(b)) => match (a.as_slice(), b.as_slice()) {
				(Some(a_s), Some(b_s)) => Self::manhattan_distance_f64_simd(a_s, b_s),
				_ => {
					let a_v: Vec<f64> = a.iter().copied().collect();
					let b_v: Vec<f64> = b.iter().copied().collect();
					Self::manhattan_distance_f64_simd(&a_v, &b_v)
				}
			},
			// f32 → widen to f64 for SIMD precision.
			(Self::F32(a), Self::F32(b)) => {
				let a_v: Vec<f64> = a.iter().map(|&x| x as f64).collect();
				let b_v: Vec<f64> = b.iter().map(|&x| x as f64).collect();
				Self::manhattan_distance_f64_simd(&a_v, &b_v)
			}
			(Self::I64(a), Self::I64(b)) => a.l1_dist(b).map(|r| r as f64).unwrap_or(f64::INFINITY),
			(Self::I32(a), Self::I32(b)) => a.l1_dist(b).map(|r| r as f64).unwrap_or(f64::INFINITY),
			(Self::I16(a), Self::I16(b)) => Self::manhattan(a, b),
			_ => f64::NAN,
		}
	}

	#[inline]
	fn minkowski<T>(a: &Array1<T>, b: &Array1<T>, order: f64) -> f64
	where
		T: ToFloat,
	{
		let dist: f64 = a
			.iter()
			.zip(b.iter())
			.map(|(a, b)| (a.to_float() - b.to_float()).abs().powf(order))
			.sum();
		dist.powf(1.0 / order)
	}

	pub(super) fn minkowski_distance(&self, other: &Self, order: f64) -> f64 {
		match (self, other) {
			(Self::F64(a), Self::F64(b)) => Self::minkowski(a, b, order),
			(Self::F32(a), Self::F32(b)) => Self::minkowski(a, b, order),
			(Self::I64(a), Self::I64(b)) => Self::minkowski(a, b, order),
			(Self::I32(a), Self::I32(b)) => Self::minkowski(a, b, order),
			(Self::I16(a), Self::I16(b)) => Self::minkowski(a, b, order),
			_ => f64::NAN,
		}
	}

	#[inline]
	fn pearson<T>(x: &Array1<T>, y: &Array1<T>) -> f64
	where
		T: ToFloat + Clone + FromPrimitive + Add<Output = T> + Div<Output = T> + Zero,
	{
		let mean_x = x.mean().expect("mean should be computable").to_float();
		let mean_y = y.mean().expect("mean should be computable").to_float();

		let mut sum_xy = 0.0;
		let mut sum_x2 = 0.0;
		let mut sum_y2 = 0.0;

		for (xi, yi) in x.iter().zip(y.iter()) {
			let diff_x = xi.to_float() - mean_x;
			let diff_y = yi.to_float() - mean_y;
			sum_xy += diff_x * diff_y;
			sum_x2 += diff_x.powi(2);
			sum_y2 += diff_y.powi(2);
		}

		let numerator = sum_xy;
		let denominator = (sum_x2 * sum_y2).sqrt();

		if denominator == 0.0 {
			return 0.0; // Return 0 if the denominator is 0
		}

		numerator / denominator
	}

	#[cfg(not(feature = "vector-hpc"))]
	fn pearson_similarity(&self, other: &Self) -> f64 {
		match (self, other) {
			(Self::F64(a), Self::F64(b)) => Self::pearson(a, b),
			(Self::F32(a), Self::F32(b)) => Self::pearson(a, b),
			(Self::I64(a), Self::I64(b)) => Self::pearson(a, b),
			(Self::I32(a), Self::I32(b)) => Self::pearson(a, b),
			(Self::I16(a), Self::I16(b)) => Self::pearson(a, b),
			_ => f64::NAN,
		}
	}

	#[cfg(feature = "vector-hpc")]
	fn pearson_similarity(&self, other: &Self) -> f64 {
		match (self, other) {
			(Self::F64(a), Self::F64(b)) => match (a.as_slice(), b.as_slice()) {
				(Some(a_s), Some(b_s)) => Self::pearson_similarity_f64_simd(a_s, b_s),
				_ => {
					let a_v: Vec<f64> = a.iter().copied().collect();
					let b_v: Vec<f64> = b.iter().copied().collect();
					Self::pearson_similarity_f64_simd(&a_v, &b_v)
				}
			},
			// f32 → widen to f64 for SIMD precision.
			(Self::F32(a), Self::F32(b)) => {
				let a_v: Vec<f64> = a.iter().map(|&x| x as f64).collect();
				let b_v: Vec<f64> = b.iter().map(|&x| x as f64).collect();
				Self::pearson_similarity_f64_simd(&a_v, &b_v)
			}
			(Self::I64(a), Self::I64(b)) => Self::pearson(a, b),
			(Self::I32(a), Self::I32(b)) => Self::pearson(a, b),
			(Self::I16(a), Self::I16(b)) => Self::pearson(a, b),
			_ => f64::NAN,
		}
	}

	fn mem_size(&self) -> usize {
		let s = match self {
			Self::F64(arr) => arr.len() * std::mem::size_of::<f64>(),
			Self::F32(arr) => arr.len() * std::mem::size_of::<f32>(),
			Self::I64(arr) => arr.len() * std::mem::size_of::<i64>(),
			Self::I32(arr) => arr.len() * std::mem::size_of::<i32>(),
			Self::I16(arr) => arr.len() * std::mem::size_of::<i16>(),
		};
		// Array1 overhead (approximately 24 bytes for ndarray metadata)
		s + 24
	}
}

/// For vectors, as we want to support very large vectors, we want to avoid copy
/// or clone. So the requirement is multiple ownership but not thread safety.
/// However, because we are running in an async context, and because we are
/// using cache structures that use the Arc as a key, the cached objects has to
/// be Sent, which then requires the use of Arc (rather than just Rc).
/// As computing the hash for a large vector is costly, this structures also
/// caches the hashcode to avoid recomputing it.
#[derive(Debug, Clone)]
pub struct SharedVector(Arc<Vector>, u64);
impl From<Vector> for SharedVector {
	fn from(v: Vector) -> Self {
		let mut h = AHasher::default();
		v.hash(&mut h);
		Self(Arc::new(v), h.finish())
	}
}

impl Deref for SharedVector {
	type Target = Vector;

	fn deref(&self) -> &Self::Target {
		&self.0
	}
}

impl Hash for SharedVector {
	fn hash<H: Hasher>(&self, state: &mut H) {
		state.write_u64(self.1);
	}
}

impl PartialEq for SharedVector {
	fn eq(&self, other: &Self) -> bool {
		self.1 == other.1 && self.0 == other.0
	}
}
impl Eq for SharedVector {}

impl SharedVector {
	pub(super) fn mem_size(&self) -> usize {
		// SharedVector stack size + Vector heap size + Arc heap overhead
		std::mem::size_of::<Self>() + self.0.mem_size() + 16
	}
}

impl Hash for Vector {
	fn hash<H: Hasher>(&self, state: &mut H) {
		match self {
			Vector::F64(v) => {
				let h = v.iter().fold(0, |acc, &x| acc ^ x.to_bits());
				state.write_u64(h);
			}
			Vector::F32(v) => {
				let h = v.iter().fold(0, |acc, &x| acc ^ x.to_bits());
				state.write_u32(h);
			}
			Vector::I64(v) => {
				let h = v.iter().fold(0, |acc, &x| acc ^ x);
				state.write_i64(h);
			}
			Vector::I32(v) => {
				let h = v.iter().fold(0, |acc, &x| acc ^ x);
				state.write_i32(h);
			}
			Vector::I16(v) => {
				let h = v.iter().fold(0, |acc, &x| acc ^ x);
				state.write_i16(h);
			}
		}
	}
}

#[cfg(test)]
impl SharedVector {
	pub(crate) fn clone_vector(&self) -> Vector {
		self.0.as_ref().clone()
	}
}

#[cfg(test)]
impl From<&Vector> for Value {
	fn from(v: &Vector) -> Self {
		let vec: Vec<_> = match v {
			Vector::F64(a) => a.iter().map(|i| Number::Float(*i)).map(Value::from).collect(),
			Vector::F32(a) => a.iter().map(|i| Number::Float(*i as f64)).map(Value::from).collect(),
			Vector::I64(a) => a.iter().map(|i| Number::Int(*i)).map(Value::from).collect(),
			Vector::I32(a) => a.iter().map(|i| Number::Int(*i as i64)).map(Value::from).collect(),
			Vector::I16(a) => a.iter().map(|i| Number::Int(*i as i64)).map(Value::from).collect(),
		};
		Value::from(vec)
	}
}

impl Vector {
	#[cfg(test)]
	pub(super) fn try_from_value(t: VectorType, d: usize, v: Value) -> Result<Self> {
		let res = match t {
			VectorType::F64 => {
				let mut vec = Vec::with_capacity(d);
				SerializedVector::check_vector_value(v, &mut vec)?;
				Vector::F64(Array1::from_vec(vec))
			}
			VectorType::F32 => {
				let mut vec = Vec::with_capacity(d);
				SerializedVector::check_vector_value(v, &mut vec)?;
				Vector::F32(Array1::from_vec(vec))
			}
			VectorType::I64 => {
				let mut vec = Vec::with_capacity(d);
				SerializedVector::check_vector_value(v, &mut vec)?;
				Vector::I64(Array1::from_vec(vec))
			}
			VectorType::I32 => {
				let mut vec = Vec::with_capacity(d);
				SerializedVector::check_vector_value(v, &mut vec)?;
				Vector::I32(Array1::from_vec(vec))
			}
			VectorType::I16 => {
				let mut vec = Vec::with_capacity(d);
				SerializedVector::check_vector_value(v, &mut vec)?;
				Vector::I16(Array1::from_vec(vec))
			}
		};
		Ok(res)
	}

	pub(super) fn try_from_vector(t: VectorType, v: &[Number]) -> Result<Self> {
		let res = match t {
			VectorType::F64 => {
				let mut vec = Vec::with_capacity(v.len());
				Self::check_vector_number(v, &mut vec)?;
				Vector::F64(Array1::from_vec(vec))
			}
			VectorType::F32 => {
				let mut vec = Vec::with_capacity(v.len());
				Self::check_vector_number(v, &mut vec)?;
				Vector::F32(Array1::from_vec(vec))
			}
			VectorType::I64 => {
				let mut vec = Vec::with_capacity(v.len());
				Self::check_vector_number(v, &mut vec)?;
				Vector::I64(Array1::from_vec(vec))
			}
			VectorType::I32 => {
				let mut vec = Vec::with_capacity(v.len());
				Self::check_vector_number(v, &mut vec)?;
				Vector::I32(Array1::from_vec(vec))
			}
			VectorType::I16 => {
				let mut vec = Vec::with_capacity(v.len());
				Self::check_vector_number(v, &mut vec)?;
				Vector::I16(Array1::from_vec(vec))
			}
		};
		Ok(res)
	}

	fn check_vector_number<T>(v: &[Number], vec: &mut Vec<T>) -> Result<()>
	where
		T: TryFrom<Number, Error = Error>,
	{
		for n in v {
			vec.push((*n).try_into()?);
		}
		Ok(())
	}

	pub(super) fn len(&self) -> usize {
		match self {
			Self::F64(v) => v.len(),
			Self::F32(v) => v.len(),
			Self::I64(v) => v.len(),
			Self::I32(v) => v.len(),
			Self::I16(v) => v.len(),
		}
	}

	pub(super) fn check_expected_dimension(current: usize, expected: usize) -> Result<()> {
		ensure!(
			current == expected,
			Error::InvalidVectorDimension {
				current,
				expected,
			}
		);
		Ok(())
	}

	pub(super) fn check_dimension(&self, expected_dim: usize) -> Result<()> {
		Self::check_expected_dimension(self.len(), expected_dim)
	}
}

impl Distance {
	pub(super) fn calculate(&self, a: &Vector, b: &Vector) -> f64 {
		match self {
			Distance::Chebyshev => a.chebyshev_distance(b),
			Distance::Cosine => a.cosine_distance(b),
			Distance::Euclidean => a.euclidean_distance(b),
			Distance::Hamming => a.hamming_distance(b),
			Distance::Jaccard => a.jaccard_similarity(b),
			Distance::Manhattan => a.manhattan_distance(b),
			Distance::Minkowski(order) => a.minkowski_distance(b, order.to_float()),
			Distance::Pearson => a.pearson_similarity(b),
		}
	}
}

#[cfg(test)]
mod tests {
	use crate::catalog::{Distance, VectorType};
	use crate::idx::trees::knn::tests::{RandomItemGenerator, get_seed_rnd, new_random_vec};
	use crate::idx::trees::vector::{SharedVector, Vector};

	fn test_distance(dist: Distance, a1: &[f64], a2: &[f64], res: f64) {
		// Convert the arrays to Vec<Number>
		let mut v1 = vec![];
		a1.iter().for_each(|&n| v1.push(n.into()));
		let mut v2 = vec![];
		a2.iter().for_each(|&n| v2.push(n.into()));

		// Check the generic distance implementation
		assert_eq!(dist.compute(&v1, &v2).unwrap(), res.into());

		// Check the "Vector" optimised implementations
		let t = VectorType::F64;
		let v1: SharedVector = Vector::try_from_vector(t, &v1).unwrap().into();
		let v2: SharedVector = Vector::try_from_vector(t, &v2).unwrap().into();
		assert_eq!(dist.calculate(&v1, &v2), res);
	}

	fn test_distance_collection(dist: Distance, size: usize, dim: usize) {
		let mut rng = get_seed_rnd();
		for vt in
			[VectorType::F64, VectorType::F32, VectorType::I64, VectorType::I32, VectorType::I16]
		{
			let r#gen = RandomItemGenerator::new(&dist, dim);
			let mut num_zero = 0;
			for i in 0..size {
				let v1 = new_random_vec(&mut rng, vt, dim, &r#gen);
				let v2 = new_random_vec(&mut rng, vt, dim, &r#gen);
				let d = dist.calculate(&v1, &v2);
				assert!(
					d.is_finite() && !d.is_nan(),
					"i: {i} - vt: {vt} - v1: {v1:?} - v2: {v2:?}"
				);
				assert_ne!(d, f64::NAN, "i: {i} - vt: {vt} - v1: {v1:?} - v2: {v2:?}");
				assert_ne!(d, f64::INFINITY, "i: {i} - vt: {vt} - v1: {v1:?} - v2: {v2:?}");
				if d == 0.0 {
					num_zero += 1;
				}
			}
			let zero_rate = num_zero as f64 / size as f64;
			assert!(zero_rate < 0.1, "vt: {vt} - zero_rate: {zero_rate}");
		}
	}

	#[test]
	fn test_distance_chebyshev() {
		test_distance_collection(Distance::Chebyshev, 100, 1536);
		test_distance(Distance::Chebyshev, &[1.0, 2.0, 3.0], &[2.0, 3.0, 4.0], 1.0);
	}

	#[test]
	fn test_distance_cosine() {
		test_distance_collection(Distance::Cosine, 100, 1536);
		test_distance(Distance::Cosine, &[1.0, 2.0, 3.0], &[2.0, 3.0, 4.0], 0.007416666029069652);
	}

	#[test]
	fn test_distance_euclidean() {
		test_distance_collection(Distance::Euclidean, 100, 1536);
		test_distance(Distance::Euclidean, &[1.0, 2.0, 3.0], &[2.0, 3.0, 4.0], 1.7320508075688772);
	}

	#[test]
	fn test_distance_hamming() {
		test_distance_collection(Distance::Hamming, 100, 1536);
		test_distance(Distance::Hamming, &[1.0, 2.0, 3.0], &[2.0, 3.0, 4.0], 3.0);
	}

	#[test]
	fn test_distance_jaccard() {
		test_distance_collection(Distance::Jaccard, 100, 768);
		test_distance(Distance::Jaccard, &[1.0, 2.0, 3.0], &[2.0, 3.0, 4.0], 0.5);
	}
	#[test]
	fn test_distance_manhattan() {
		test_distance_collection(Distance::Manhattan, 100, 1536);
		test_distance(Distance::Manhattan, &[1.0, 2.0, 3.0], &[2.0, 3.0, 4.0], 3.0);
	}
	#[test]
	fn test_distance_minkowski() {
		test_distance_collection(Distance::Minkowski(3.into()), 100, 1536);
		test_distance(
			Distance::Minkowski(3.into()),
			&[1.0, 2.0, 3.0],
			&[2.0, 3.0, 4.0],
			1.4422495703074083,
		);
	}

	#[test]
	fn test_distance_pearson() {
		test_distance_collection(Distance::Pearson, 100, 1536);
		test_distance(Distance::Pearson, &[1.0, 2.0, 3.0], &[2.0, 3.0, 4.0], 1.0);
	}
}

#[cfg(all(test, feature = "vector-hpc"))]
mod hpc_tests {
	use ndarray::Array1;
	use super::*;

	/// SIMD cosine must agree with the scalar fallback to within fp tolerance,
	/// across the dim sizes that matter (1, 128, 384, 768, 1024).
	#[test]
	fn test_cosine_hpc_matches_scalar() {
		// We can't easily call the scalar version from inside the feature-gated
		// module, so we compute scalar inline.
		fn scalar_cosine(a: &[f64], b: &[f64]) -> f64 {
			let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
			let na: f64 = a.iter().map(|x| x * x).sum::<f64>().sqrt();
			let nb: f64 = b.iter().map(|y| y * y).sum::<f64>().sqrt();
			1.0 - dot / (na * nb)
		}

		let mut state: u64 = 0xC0FFEE;
		fn lcg(s: &mut u64) -> f64 {
			*s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
			((*s >> 11) as f64) / ((1u64 << 53) as f64) - 0.5
		}

		for &dim in &[1usize, 128, 384, 768, 1024] {
			let a_v: Vec<f64> = (0..dim).map(|_| lcg(&mut state)).collect();
			let b_v: Vec<f64> = (0..dim).map(|_| lcg(&mut state)).collect();
			let a = Array1::from_vec(a_v.clone());
			let b = Array1::from_vec(b_v.clone());

			let simd = Vector::cosine_distance_f64(&a, &b);
			let scalar = scalar_cosine(&a_v, &b_v);

			assert!(
				(simd - scalar).abs() < 1e-9,
				"dim {}: simd={}, scalar={}, diff={}",
				dim,
				simd,
				scalar,
				(simd - scalar).abs(),
			);
		}
	}

	/// SIMD Euclidean must agree with the scalar fallback to within fp
	/// tolerance, across the same dim sizes.
	#[test]
	fn test_euclidean_hpc_matches_scalar() {
		fn scalar_l2(a: &[f64], b: &[f64]) -> f64 {
			a.iter().zip(b).map(|(x, y)| (x - y).powi(2)).sum::<f64>().sqrt()
		}

		let mut state: u64 = 0xBADC0FFEE;
		fn lcg(s: &mut u64) -> f64 {
			*s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
			((*s >> 11) as f64) / ((1u64 << 53) as f64) - 0.5
		}

		for &dim in &[1usize, 128, 384, 768, 1024] {
			let a_v: Vec<f64> = (0..dim).map(|_| lcg(&mut state)).collect();
			let b_v: Vec<f64> = (0..dim).map(|_| lcg(&mut state)).collect();

			let simd = Vector::euclidean_distance_f64_simd(&a_v, &b_v);
			let scalar = scalar_l2(&a_v, &b_v);

			assert!(
				(simd - scalar).abs() < 1e-9,
				"dim {}: simd={}, scalar={}, diff={}",
				dim,
				simd,
				scalar,
				(simd - scalar).abs(),
			);
		}
	}

	/// SIMD Manhattan must agree with the scalar fallback to within fp tolerance,
	/// across the same dim sizes.
	#[test]
	fn test_manhattan_hpc_matches_scalar() {
		fn scalar_manhattan(a: &[f64], b: &[f64]) -> f64 {
			a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum()
		}

		let mut state: u64 = 0xDEADBEEF;
		fn lcg(s: &mut u64) -> f64 {
			*s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
			((*s >> 11) as f64) / ((1u64 << 53) as f64) - 0.5
		}

		for &dim in &[1usize, 128, 384, 768, 1024] {
			let a_v: Vec<f64> = (0..dim).map(|_| lcg(&mut state)).collect();
			let b_v: Vec<f64> = (0..dim).map(|_| lcg(&mut state)).collect();

			let simd = Vector::manhattan_distance_f64_simd(&a_v, &b_v);
			let scalar = scalar_manhattan(&a_v, &b_v);

			assert!(
				(simd - scalar).abs() < 1e-9,
				"dim {}: simd={}, scalar={}, diff={}",
				dim,
				simd,
				scalar,
				(simd - scalar).abs(),
			);
		}
	}

	/// SIMD Chebyshev must agree with the scalar fallback to within fp tolerance,
	/// across the same dim sizes.
	#[test]
	fn test_chebyshev_hpc_matches_scalar() {
		fn scalar_chebyshev(a: &[f64], b: &[f64]) -> f64 {
			a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(f64::NEG_INFINITY, f64::max)
		}

		let mut state: u64 = 0xFEEDFACE;
		fn lcg(s: &mut u64) -> f64 {
			*s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
			((*s >> 11) as f64) / ((1u64 << 53) as f64) - 0.5
		}

		for &dim in &[1usize, 128, 384, 768, 1024] {
			let a_v: Vec<f64> = (0..dim).map(|_| lcg(&mut state)).collect();
			let b_v: Vec<f64> = (0..dim).map(|_| lcg(&mut state)).collect();

			let simd = Vector::chebyshev_distance_f64_simd(&a_v, &b_v);
			let scalar = scalar_chebyshev(&a_v, &b_v);

			assert!(
				(simd - scalar).abs() < 1e-9,
				"dim {}: simd={}, scalar={}, diff={}",
				dim,
				simd,
				scalar,
				(simd - scalar).abs(),
			);
		}
	}

	/// SIMD Pearson correlation must agree with the scalar fallback to within fp
	/// tolerance, across the same dim sizes.
	#[test]
	fn test_pearson_hpc_matches_scalar() {
		fn scalar_pearson(a: &[f64], b: &[f64]) -> f64 {
			let n = a.len() as f64;
			let mean_a = a.iter().sum::<f64>() / n;
			let mean_b = b.iter().sum::<f64>() / n;
			let dot: f64 = a.iter().zip(b).map(|(x, y)| (x - mean_a) * (y - mean_b)).sum();
			let na: f64 = a.iter().map(|x| (x - mean_a).powi(2)).sum::<f64>().sqrt();
			let nb: f64 = b.iter().map(|y| (y - mean_b).powi(2)).sum::<f64>().sqrt();
			if na * nb < 1e-12 {
				0.0
			} else {
				dot / (na * nb)
			}
		}

		let mut state: u64 = 0xCAFEBABE;
		fn lcg(s: &mut u64) -> f64 {
			*s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
			((*s >> 11) as f64) / ((1u64 << 53) as f64) - 0.5
		}

		for &dim in &[1usize, 128, 384, 768, 1024] {
			let a_v: Vec<f64> = (0..dim).map(|_| lcg(&mut state)).collect();
			let b_v: Vec<f64> = (0..dim).map(|_| lcg(&mut state)).collect();

			let simd = Vector::pearson_similarity_f64_simd(&a_v, &b_v);
			let scalar = scalar_pearson(&a_v, &b_v);

			// Pearson is two-pass (mean subtraction + cosine), so allow 1e-9.
			assert!(
				(simd - scalar).abs() < 1e-9,
				"dim {}: simd={}, scalar={}, diff={}",
				dim,
				simd,
				scalar,
				(simd - scalar).abs(),
			);
		}
	}
}
