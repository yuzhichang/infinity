// Copyright(C) 2026 InfiniFlow, Inc. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Mean-centered i8 scalar quantization (LVQ).
//!
//! Port of `src/storage/knn_index/knn_hnsw/data_store/lvq_vec_store.cppm` and
//! the LVQ distance kernels in `dist_func_{l2,cos,ip}.cppm`.
//!
//! A vector is stored as `Q = scale * code + bias + mean`, where `code` is an
//! i8 per dimension and `scale`/`bias` are chosen from the vector's own range
//! after subtracting the store-wide mean. Distances are computed from the
//! integer dot product plus the per-vector local cache and the mean's global
//! cache, exactly as the reference does.

/// The metric decides the local/global cache contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metric {
    L2,
    Ip,
    Cos,
}

/// Where a record's `i8` code lives.
///
/// `Owned` while a store is being built (compressed one vector at a time);
/// `Mapped` once it is loaded, where the codes are already `i8` bytes inside the
/// chunk mapping and copying them out means one `Vec` per vector - 8.34M
/// allocations on the L scenario, for data that never changes.
///
/// The `Arc` is the same device `vector::MappedFloats` uses: an index is shared
/// by `Arc<Store>` across build tasks and held for a segment's lifetime, so a
/// borrowed slice would need a lifetime parameter on every type that mentions
/// an index, while the `Arc` states "the file outlives me" and costs a refcount.
#[derive(Clone, Debug)]
pub enum Code {
    Owned(Vec<i8>),
    Mapped(CodeMapper),
}

impl Default for Code {
    fn default() -> Self {
        Self::Owned(Vec::new())
    }
}

impl PartialEq for Code {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Code {
    pub fn as_slice(&self) -> &[i8] {
        match self {
            Self::Owned(v) => v,
            Self::Mapped(m) => m.as_slice(),
        }
    }
}

/// The mean vector: owned while building, a window once loaded.
#[derive(Clone, Debug)]
pub enum MeanVec {
    Owned(Vec<f64>),
    Mapped(MappedF64s),
}

impl PartialEq for MeanVec {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl std::ops::Deref for MeanVec {
    type Target = [f64];

    fn deref(&self) -> &[f64] {
        match self {
            Self::Owned(v) => v,
            Self::Mapped(m) => m,
        }
    }
}

/// A `f64` array window into a mapping that outlives its owner.
///
/// The mean vector: `dim` `f64`s, which the chunk stores as those doubles.
#[derive(Clone)]
pub struct MappedF64s {
    _owner: std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>,
    ptr: *const f64,
    len: usize,
}

impl std::fmt::Debug for MappedF64s {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedF64s")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl MappedF64s {
    pub fn new(owner: std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>, bytes: &[u8]) -> Option<Self> {
        let v: &[f64] = bytemuck::try_cast_slice(bytes).ok()?;
        Some(Self {
            _owner: owner,
            ptr: v.as_ptr(),
            len: v.len(),
        })
    }
}

#[allow(unsafe_code)]
impl std::ops::Deref for MappedF64s {
    type Target = [f64];

    fn deref(&self) -> &[f64] {
        // SAFETY: `_owner` keeps the mapping alive for `&self`; the bytes are
        // immutable; `ptr`/`len` were checked by `bytemuck`.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

#[allow(unsafe_code)]
unsafe impl Send for MappedF64s {}
#[allow(unsafe_code)]
unsafe impl Sync for MappedF64s {}

/// An `f32` array window into a mapping that outlives its owner.
///
/// `Deref<Target = [f32]>`; the point is that a store loaded from a chunk reads
/// the matrix and centroid it was trained with instead of decoding them.
#[derive(Clone)]
pub struct MappedF32s {
    _owner: std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>,
    ptr: *const f32,
    len: usize,
}

impl std::fmt::Debug for MappedF32s {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedF32s")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl MappedF32s {
    /// A window over `bytes`, which must be `f32`s inside `owner`'s mapping.
    /// `None` for a misaligned buffer, which is what `bytemuck` rejects.
    pub fn new(owner: std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>, bytes: &[u8]) -> Option<Self> {
        let floats: &[f32] = bytemuck::try_cast_slice(bytes).ok()?;
        Some(Self {
            _owner: owner,
            ptr: floats.as_ptr(),
            len: floats.len(),
        })
    }
}

#[allow(unsafe_code)]
impl std::ops::Deref for MappedF32s {
    type Target = [f32];

    fn deref(&self) -> &[f32] {
        // SAFETY: `_owner` keeps the mapping alive for `&self`; the bytes are
        // immutable; `ptr`/`len` were checked by `bytemuck`.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

#[allow(unsafe_code)]
unsafe impl Send for MappedF32s {}
#[allow(unsafe_code)]
unsafe impl Sync for MappedF32s {}

/// RaBitQ's sign bits: one `u8` per eight dimensions.
///
/// Owned while a store is built; a window into the chunk mapping once loaded -
/// the bits need no decoding, so that is the whole cost of a load.
#[derive(Clone, Debug, PartialEq)]
pub enum CodeBytes {
    Owned(Vec<u8>),
    Mapped(CodeByteWindow),
}

impl Default for CodeBytes {
    fn default() -> Self {
        Self::Owned(Vec::new())
    }
}

impl std::ops::Deref for CodeBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl CodeBytes {
    pub fn owned(bytes: Vec<u8>) -> Self {
        Self::Owned(bytes)
    }

    pub fn mapped(owner: std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>, bytes: &[u8]) -> Self {
        Self::Mapped(CodeByteWindow {
            _owner: owner,
            ptr: bytes.as_ptr(),
            len: bytes.len(),
        })
    }

    pub fn as_slice(&self) -> &[u8] {
        match self {
            Self::Owned(v) => v,
            Self::Mapped(m) => m.as_slice(),
        }
    }
}

#[derive(Clone)]
pub struct CodeByteWindow {
    _owner: std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>,
    ptr: *const u8,
    len: usize,
}

impl std::fmt::Debug for CodeByteWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeByteWindow")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl PartialEq for CodeByteWindow {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl CodeByteWindow {
    #[allow(unsafe_code)]
    fn as_slice(&self) -> &[u8] {
        // SAFETY: `_owner` keeps the mapping alive for `&self`, and the bytes
        // are immutable.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

#[allow(unsafe_code)]
unsafe impl Send for CodeByteWindow {}
#[allow(unsafe_code)]
unsafe impl Sync for CodeByteWindow {}

/// An `i8` window into a mapping that outlives the store.
///
/// SAFETY note: `_owner` keeps the pages mapped for as long as `ptr` is
/// reachable, and a chunk file is published by rename rather than written in
/// place, so the bytes behind `ptr` never change under a reader. `ptr`/`len`
/// came from `bytemuck` in `CodeMapper::new`.
#[derive(Clone)]
pub struct CodeMapper {
    _owner: std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>,
    ptr: *const i8,
    len: usize,
}

impl std::fmt::Debug for CodeMapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodeMapper")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl CodeMapper {
    /// A window over `bytes`, which must be the mapping `owner` holds.
    ///
    /// # Panics
    ///
    /// Never: `bytemuck` rejects only a misaligned pointer, and `i8` accepts
    /// every alignment, so the cast cannot fail.
    fn new(owner: std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>, bytes: &[i8]) -> Self {
        let (ptr, len) = (bytes.as_ptr(), bytes.len());
        Self {
            _owner: owner,
            ptr,
            len,
        }
    }

    #[allow(unsafe_code)]
    pub fn as_slice(&self) -> &[i8] {
        // SAFETY: see the type's note - `_owner` is alive for `&self` and the
        // bytes are immutable.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

#[allow(unsafe_code)]
unsafe impl Send for CodeMapper {}
#[allow(unsafe_code)]
unsafe impl Sync for CodeMapper {}

/// The 16-byte numeric prefix of a record: `scale`, `bias`, `local.0`,
/// `local.1`, in that order and with no padding - the layout the file uses.
///
/// Referenced rather than copied for the same reason [`Code`] is: a loaded store
/// holds 8.34M of these, so four owned `f32` fields are 334 MB of heap that the
/// mapping already contains. `repr(C)` is what makes the reference sound; the
/// file is written by `records_bytes`, which emits the four `to_le_bytes` in
/// this order.
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct LvqHeader {
    pub scale: f32,
    pub bias: f32,
    /// `(norm1 * scale, norm2 * scale^2)` for L2, `(norm1 * scale,
    /// <mean, code> * scale)` for cosine/inner product.
    pub local_0: f32,
    pub local_1: f32,
}

impl LvqHeader {
    pub fn local(&self) -> (f32, f32) {
        (self.local_0, self.local_1)
    }
}

/// One compressed vector: the C++ `LVQData<f32, pair<f32,f32>, i8>` record.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LvqData {
    pub header: Header,
    /// `(norm1 * scale, norm2 * scale^2)` for L2, `(norm1 * scale,
    /// <mean, code> * scale)` for cosine/inner product, as `header.local()`.
    pub code: Code,
}

/// Where a record's numeric prefix lives: owned while a store is built, a
/// window into the mapping once it is loaded.
#[derive(Clone, Debug)]
pub enum Header {
    Owned(LvqHeader),
    /// A window onto one record's 16-byte prefix in the mapping. Records are
    /// `prefix || codes`, so the prefixes are not contiguous and cannot be cast
    /// as one array; each record keeps its own pointer into the shared owner.
    Mapped(HeaderMapper),
}

/// A `LvqHeader` window into the mapping that outlives the store.
///
/// SAFETY note: `_owner` keeps the pages mapped while `ptr` is reachable, the
/// bytes are immutable (a chunk is published by rename), and the cast was
/// checked by `bytemuck` in [`Self::new`].
#[derive(Clone)]
pub struct HeaderMapper {
    _owner: std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>,
    ptr: *const LvqHeader,
}

impl std::fmt::Debug for HeaderMapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeaderMapper").finish_non_exhaustive()
    }
}

impl HeaderMapper {
    fn new(owner: std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>, bytes: &[u8]) -> Option<Self> {
        // `LvqHeader` is `repr(C)` over four `f32`, so this is the same 16 bytes
        // the file holds, and `bytemuck` requires the 4-byte alignment an
        // `f32` needs.
        let ptr = bytemuck::try_from_bytes::<LvqHeader>(bytes).ok()? as *const LvqHeader;
        Some(Self { _owner: owner, ptr })
    }

    #[allow(unsafe_code)]
    fn get(&self) -> &LvqHeader {
        // SAFETY: see the type's note.
        unsafe { &*self.ptr }
    }
}

#[allow(unsafe_code)]
unsafe impl Send for HeaderMapper {}
#[allow(unsafe_code)]
unsafe impl Sync for HeaderMapper {}

impl Default for Header {
    fn default() -> Self {
        Self::Owned(LvqHeader::default())
    }
}

impl PartialEq for Header {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl std::ops::Deref for Header {
    type Target = LvqHeader;

    fn deref(&self) -> &LvqHeader {
        match self {
            Self::Owned(h) => h,
            Self::Mapped(m) => m.get(),
        }
    }
}

/// A compressed query: the reference's `LVQQuery`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LvqQuery {
    data: LvqData,
}

impl LvqQuery {
    pub fn data(&self) -> &LvqData {
        &self.data
    }
}

/// The LVQ store of one chunk.
#[derive(Clone, Debug, PartialEq)]
pub struct LvqStore {
    dim: usize,
    metric: Metric,
    normalize: bool,
    /// `MeanType` in the reference is `double`. A window into the chunk mapping
    /// once loaded.
    mean: MeanVec,
    /// `(sum(mean), sum(mean^2))`, used by cosine/inner product.
    global: Option<(f64, f64)>,
    data: Records,
}

/// The compressed records of a store.
///
/// `Owned` while building. `Mapped` once loaded: every record is
/// `16 + dim` bytes, so record `i` starts at `i * record_size` and can be read
/// out of the mapping on demand. Holding a `Vec<LvqData>` instead meant 509 MB
/// of descriptors for 8.34M records - two `Arc` pointers and two length fields
/// per record, describing data that is already laid out in the file.
#[derive(Clone, Debug, PartialEq)]
enum Records {
    Owned(Vec<LvqData>),
    Mapped(MappedRecords),
}

/// A loaded store's records: the mapping, plus what is needed to address one.
#[derive(Clone)]
struct MappedRecords {
    owner: std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>,
    /// Byte offset of record 0 within the mapping.
    at: usize,
    record_size: usize,
    dim: usize,
    n: usize,
}

impl MappedRecords {
    fn owner(&self) -> std::sync::Arc<dyn AsRef<[u8]> + Send + Sync> {
        std::sync::Arc::clone(&self.owner)
    }

    /// The record at `index`, with both halves pointing into the mapping.
    fn record(&self, index: usize) -> Option<LvqData> {
        if index >= self.n {
            return None;
        }
        let bytes = self.owner.as_ref().as_ref();
        let start = self.at + index * self.record_size;
        let prefix = bytes.get(start..start + 16)?;
        let codes = bytes.get(start + 16..start + 16 + self.dim)?;
        let header = HeaderMapper::new(self.owner(), prefix)?;
        let code = CodeMapper::new(self.owner(), bytemuck::cast_slice(codes));
        Some(LvqData {
            header: Header::Mapped(header),
            code: Code::Mapped(code),
        })
    }
}

impl std::fmt::Debug for MappedRecords {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedRecords")
            .field("n", &self.n)
            .field("record_size", &self.record_size)
            .finish_non_exhaustive()
    }
}

impl PartialEq for MappedRecords {
    fn eq(&self, other: &Self) -> bool {
        self.n == other.n && self.record_size == other.record_size && self.dim == other.dim
    }
}

impl Records {
    fn len(&self) -> usize {
        match self {
            Self::Owned(v) => v.len(),
            Self::Mapped(m) => m.n,
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn get(&self, index: usize) -> Option<LvqData> {
        match self {
            Self::Owned(v) => v.get(index).cloned(),
            Self::Mapped(m) => m.record(index),
        }
    }

    fn as_owned(&self) -> Option<&[LvqData]> {
        match self {
            Self::Owned(v) => Some(v),
            Self::Mapped(_) => None,
        }
    }
}

impl LvqStore {
    /// C++ `LVQVecStoreMeta::Make(dim, normalize)`: a zero mean and the
    /// matching global cache.
    pub fn new(dim: usize, metric: Metric, normalize: bool) -> Self {
        let mean = vec![0.0; dim];
        let global = match metric {
            Metric::L2 => None,
            Metric::Ip | Metric::Cos => Some(global_cache(&mean)),
        };
        Self {
            dim,
            metric,
            normalize,
            mean: MeanVec::Owned(mean),
            global,
            data: Records::Owned(Vec::new()),
        }
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn metric(&self) -> Metric {
        self.metric
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn mean(&self) -> &[f64] {
        &self.mean
    }

    /// The records, when they are owned. A mapped store has no slice to hand
    /// out, so callers that need one go through [`Self::record`].
    pub fn data(&self) -> Option<&[LvqData]> {
        self.data.as_owned()
    }

    /// The record of one stored vector, borrowed (C++ `GetVec` hands out a
    /// `const LVQData *`). `None` when `index` is out of range.
    pub fn record(&self, index: usize) -> Option<LvqData> {
        self.data.get(index)
    }

    /// Bytes one compressed vector occupies (C++ `compress_data_size`).
    pub fn record_size(&self) -> usize {
        16 + self.dim
    }

    fn normalize_vec(&self, v: &[f32]) -> Vec<f32> {
        if !self.normalize {
            return v.to_vec();
        }
        let norm = v
            .iter()
            .map(|x| f64::from(*x) * f64::from(*x))
            .sum::<f64>()
            .sqrt();
        if norm == 0.0 {
            return vec![0.0; v.len()];
        }
        v.iter().map(|x| (*x as f64 / norm) as f32).collect()
    }

    /// C++ `CompressTo`: mean-center, scalar-quantize to i8 and build the
    /// local cache. `v` must have `dim` elements.
    pub fn compress(&mut self, v: &[f32]) -> &LvqData {
        let src = self.normalize_vec(v);
        let mut lower = f32::MAX;
        let mut upper = -f32::MAX;
        for (j, x) in src.iter().enumerate() {
            let x = *x - self.mean[j] as f32;
            lower = lower.min(x);
            upper = upper.max(x);
        }
        let scale = (upper - lower) / 255.0;
        let bias = lower + 128.0 * scale;
        let mut code = vec![0i8; self.dim];
        if scale != 0.0 {
            let inv = 1.0 / scale;
            for (j, x) in src.iter().enumerate() {
                let c = ((*x - self.mean[j] as f32 - bias) * inv + 0.5).floor();
                code[j] = c.clamp(-128.0, 127.0) as i8;
            }
        }
        let local = self.local_cache(&code, scale);
        let data = LvqData {
            header: Header::Owned(LvqHeader {
                scale,
                bias,
                local_0: local.0,
                local_1: local.1,
            }),
            code: Code::Owned(code),
        };
        match &mut self.data {
            Records::Owned(v) => {
                v.push(data);
                v.last().unwrap()
            }
            Records::Mapped(_) => {
                // A mapped store is read-only; compressing into it is a caller
                // bug (the loaded form is never appended to).
                unreachable!("compress into a mapped LVQ store")
            }
        }
    }

    fn local_cache(&self, code: &[i8], scale: f32) -> (f32, f32) {
        match self.metric {
            Metric::L2 => {
                let mut norm1 = 0i64;
                let mut norm2 = 0i64;
                for c in code {
                    norm1 += i64::from(*c);
                    norm2 += i64::from(*c) * i64::from(*c);
                }
                (norm1 as f32 * scale, norm2 as f32 * scale * scale)
            }
            Metric::Ip | Metric::Cos => {
                let mut norm1 = 0i64;
                let mut mean_c = 0.0f64;
                for (j, c) in code.iter().enumerate() {
                    norm1 += i64::from(*c);
                    mean_c += self.mean[j] * f64::from(*c);
                }
                (norm1 as f32 * scale, (mean_c as f32) * scale)
            }
        }
    }

    /// Sets the mean/global cache without compressing any vector
    /// (`OptAddVec`'s training half).
    pub fn train<V: AsRef<[f32]>>(&mut self, vectors: &[V]) {
        if vectors.is_empty() {
            return;
        }
        let mut mean = vec![0.0f64; self.dim];
        for v in vectors {
            for (j, x) in v.as_ref().iter().enumerate() {
                if j < mean.len() {
                    mean[j] += *x as f64;
                }
            }
        }
        let n = vectors.len() as f64;
        for m in &mut mean {
            *m /= n;
        }
        self.mean = MeanVec::Owned(mean);
        self.global = match self.metric {
            Metric::L2 => None,
            Metric::Ip | Metric::Cos => Some(global_cache(&self.mean)),
        };
    }

    /// C++ `LVQVecStoreMeta::Optimize`: recenter the store on `vectors` and
    /// recompress every vector.
    pub fn build(&mut self, vectors: &[Vec<f32>]) {
        self.train(vectors);
        self.data = Records::Owned(Vec::new());
        for v in vectors {
            self.compress(v);
        }
    }

    /// A query built from a stored vector's own code (C++ `GetVecToQuery`).
    pub fn query_at(&self, index: usize) -> Option<LvqQuery> {
        self.data
            .get(index)
            .map(|data| LvqQuery { data: data.clone() })
    }

    /// C++ `LVQVecStoreMeta::MakeQuery`.
    pub fn make_query(&self, v: &[f32]) -> LvqQuery {
        let src = self.normalize_vec(v);
        let mut lower = f32::MAX;
        let mut upper = -f32::MAX;
        for (j, x) in src.iter().enumerate() {
            let x = *x - self.mean[j] as f32;
            lower = lower.min(x);
            upper = upper.max(x);
        }
        let scale = (upper - lower) / 255.0;
        let bias = lower + 128.0 * scale;
        let mut code = vec![0i8; self.dim];
        if scale != 0.0 {
            let inv = 1.0 / scale;
            for (j, x) in src.iter().enumerate() {
                let c = ((*x - self.mean[j] as f32 - bias) * inv + 0.5).floor();
                code[j] = c.clamp(-128.0, 127.0) as i8;
            }
        }
        let local = self.local_cache(&code, scale);
        LvqQuery {
            data: LvqData {
                header: Header::Owned(LvqHeader {
                    scale,
                    bias,
                    local_0: local.0,
                    local_1: local.1,
                }),
                code: Code::Owned(code),
            },
        }
    }

    /// C++ `DecompressTo`: `scale * code + bias + mean`.
    pub fn decompress(&self, index: usize) -> Option<Vec<f64>> {
        let d = self.data.get(index)?;
        Some(
            d.code
                .as_slice()
                .iter()
                .enumerate()
                .map(|(j, c)| {
                    f64::from(d.header.scale) * f64::from(*c)
                        + f64::from(d.header.bias)
                        + self.mean[j]
                })
                .collect(),
        )
    }

    /// The LVQ distance used by the HNSW graph (smaller is closer):
    /// squared L2, or the negated cosine/inner product.
    pub fn distance(&self, query: &LvqQuery, index: usize) -> Option<f32> {
        let v = self.data.get(index)?;
        Some(self.distance_between(&query.data, &v))
    }

    /// The distance between two records already in the store, borrowing both.
    ///
    /// The graph builder compares a vertex with its neighbours and with the
    /// candidates of `SelectNeighborsHeuristic` thousands of times; the
    /// reference hands `LVQDist` two `const LVQData *` and never copies, while
    /// building an `LvqQuery` here would clone the whole `Vec<i8>` code per
    /// comparison.
    pub fn distance_between(&self, q: &LvqData, v: &LvqData) -> f32 {
        // The reference's `I8IPSSEResidual` (`LVQDist` calls it): a 16-lane i8
        // dot product, exact in `i32`. This is the build's hottest function -
        // 706 of 1127 thread samples sat here on a SIFT 1M build - so it runs
        // the vectorized kernel rather than a scalar `map().sum()`.
        let ip = i8_ip_residual(q.code.as_slice(), v.code.as_slice());
        match self.metric {
            Metric::L2 => {
                let beta = q.header.bias - v.header.bias;
                q.header.local_1 + v.header.local_1 + beta * beta * self.dim as f32
                    - 2.0 * q.header.scale * v.header.scale * ip as f32
                    + 2.0 * beta * q.header.local_0
                    - 2.0 * beta * v.header.local_0
            }
            Metric::Ip | Metric::Cos => {
                let (n1_mean, n2_mean) = self.global.unwrap_or((0.0, 0.0));
                let dist = f64::from(q.header.scale) * f64::from(v.header.scale) * f64::from(ip)
                    + f64::from(v.header.bias) * f64::from(q.header.local_0)
                    + f64::from(q.header.bias) * f64::from(v.header.local_0)
                    + self.dim as f64 * f64::from(q.header.bias) * f64::from(v.header.bias)
                    + n2_mean
                    + (f64::from(q.header.bias) + f64::from(v.header.bias)) * n1_mean
                    + f64::from(q.header.local_1)
                    + f64::from(v.header.local_1);
                -(dist as f32)
            }
        }
    }

    /// C++ `LVQVecStoreMeta::Save`: dim, mean and the metric's global cache.
    pub fn meta_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + 8 * self.dim + 16);
        out.extend_from_slice(&(self.dim as u64).to_le_bytes());
        for m in self.mean.iter() {
            out.extend_from_slice(&m.to_le_bytes());
        }
        if let Some((a, b)) = self.global {
            match self.metric {
                Metric::Ip => {
                    out.extend_from_slice(&a.to_le_bytes());
                    out.extend_from_slice(&b.to_le_bytes());
                }
                Metric::Cos => {
                    out.extend_from_slice(&(a as f32).to_le_bytes());
                    out.extend_from_slice(&(b as f32).to_le_bytes());
                }
                Metric::L2 => {}
            }
        }
        out
    }

    /// C++ `LVQVecStoreInner::Save`: the per-vector records.
    pub fn records_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len() * self.record_size());
        for d in self.data.as_owned().unwrap_or(&[]) {
            out.extend_from_slice(&d.header.scale.to_le_bytes());
            out.extend_from_slice(&d.header.bias.to_le_bytes());
            out.extend_from_slice(&d.header.local_0.to_le_bytes());
            out.extend_from_slice(&d.header.local_1.to_le_bytes());
            for c in d.code.as_slice() {
                out.push(*c as u8);
            }
        }
        out
    }

    /// C++ `LVQVecStoreMeta::Save` + `Inner::Save`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = self.meta_bytes();
        out.extend_from_slice(&self.records_bytes());
        out
    }

    /// Loads the meta blob alone (C++ `LoadFromPtr`).
    pub fn from_meta(metric: Metric, normalize: bool, bytes: &[u8]) -> Option<(Self, usize)> {
        Self::from_meta_with(metric, normalize, bytes, None)
    }

    /// [`Self::from_meta`] borrowing the mean from `backing`.
    pub fn from_meta_with(
        metric: Metric,
        normalize: bool,
        bytes: &[u8],
        mean_backing: Option<std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>>,
    ) -> Option<(Self, usize)> {
        if bytes.len() < 8 {
            return None;
        }
        let dim = u64::from_le_bytes(bytes[0..8].try_into().ok()?) as usize;
        let mut at = 8;
        let mean_bytes = dim.checked_mul(8)?;
        if bytes.len() < at + mean_bytes {
            return None;
        }
        let mean_bytes_slice = bytes.get(at..at + mean_bytes)?;
        at += mean_bytes;
        let global = match metric {
            Metric::L2 => None,
            Metric::Ip => {
                if bytes.len() < at + 16 {
                    return None;
                }
                let a = f64::from_le_bytes(bytes[at..at + 8].try_into().ok()?);
                let b = f64::from_le_bytes(bytes[at + 8..at + 16].try_into().ok()?);
                at += 16;
                Some((a, b))
            }
            Metric::Cos => {
                if bytes.len() < at + 8 {
                    return None;
                }
                let a = f32::from_le_bytes(bytes[at..at + 4].try_into().ok()?);
                let b = f32::from_le_bytes(bytes[at + 4..at + 8].try_into().ok()?);
                at += 8;
                Some((f64::from(a), f64::from(b)))
            }
        };
        let mean = match mean_backing {
            Some(owner) => MeanVec::Mapped(MappedF64s::new(owner, mean_bytes_slice)?),
            None => MeanVec::Owned(
                mean_bytes_slice
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|c| f64::from_le_bytes(*c))
                    .collect(),
            ),
        };
        Some((
            Self {
                dim,
                metric,
                normalize,
                mean,
                global,
                data: Records::Owned(Vec::new()),
            },
            at,
        ))
    }

    /// Loads the per-vector records into an existing meta.
    /// Loads `n` records, borrowing their codes from `backing` when one is
    /// given.
    ///
    /// Each record is `16` bytes of `scale`/`bias`/`local` followed by `dim`
    /// `i8` codes. Only the 16 bytes are decoded; with `backing` the codes are a
    /// window into the mapping, which is what keeps a load from allocating one
    /// `Vec<i8>` per vector - 8.34M of them on the L scenario, for bytes that
    /// are already `i8` and never change. Without it (an owned buffer, which is
    /// what the tests and the builder pass) the codes are copied.
    pub fn load_records(
        &mut self,
        records: &[u8],
        backing: Option<std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>>,
        n: usize,
    ) -> Option<()> {
        let record = self.record_size();
        if records.len() < n * record {
            return None;
        }
        // With a mapping, nothing is read here at all: `Records::Mapped` keeps
        // the offset and the stride, and `record(i)` builds the two windows when
        // someone asks for record `i`. That is what a loaded store holds - a few
        // numbers per store instead of 64 bytes per record.
        if let Some(owner) = backing {
            let base = owner.as_ref().as_ref().as_ptr() as usize;
            let at = records.as_ptr() as usize - base;
            self.data = Records::Mapped(MappedRecords {
                owner,
                at,
                record_size: record,
                dim: self.dim,
                n,
            });
            return Some(());
        }
        let mut at = 0;
        self.data = Records::Owned(Vec::with_capacity(n));
        for _ in 0..n {
            let prefix = &records[at..at + 16];
            at += 16;
            let raw = &records[at..at + self.dim];
            at += self.dim;
            let header: LvqHeader = bytemuck::pod_read_unaligned(prefix);
            let codes: &[i8] = bytemuck::cast_slice(raw);
            let owned = match &mut self.data {
                Records::Owned(v) => v,
                Records::Mapped(_) => unreachable!("just assigned"),
            };
            owned.push(LvqData {
                header: Header::Owned(header),
                code: Code::Owned(codes.to_vec()),
            });
        }
        Some(())
    }

    /// Loads a complete store from the meta and record blobs.
    pub fn from_parts(
        metric: Metric,
        normalize: bool,
        meta: &[u8],
        records: &[u8],
        // The mapping `records` points into, when there is one; see
        // `load_records` for what passing it avoids.
        backing: Option<std::sync::Arc<dyn AsRef<[u8]> + Send + Sync>>,
        n: usize,
    ) -> Option<Self> {
        let (mut store, _) = Self::from_meta_with(metric, normalize, meta, backing.clone())?;
        store.load_records(records, backing, n)?;
        Some(store)
    }

    /// Loads a meta-and-records blob (the standalone unit-test layout).
    pub fn from_bytes(
        metric: Metric,
        normalize: bool,
        bytes: &[u8],
        n: usize,
    ) -> Option<(Self, usize)> {
        let (mut store, at) = Self::from_meta(metric, normalize, bytes)?;
        let record = store.record_size();
        store.load_records(&bytes[at..], None, n)?;
        Some((store, at + n * record))
    }
}

fn global_cache(mean: &[f64]) -> (f64, f64) {
    let mut n1 = 0.0;
    let mut n2 = 0.0;
    for m in mean.iter() {
        n1 += m;
        n2 += m * m;
    }
    (n1, n2)
}

/// C++ `I8IPSSEResidual`: the exact int32 dot product of two i8 vectors.
///
/// The reference runs `_mm_maddubs_epi16` over 16 `i8` at a time, accumulating
/// in `int32` and finishing the tail (`dim & 15`) with `I8IPBF`. Because every
/// product of two `i8` fits in `i16` and the sum of up to 16 of those fits in
/// `i32`, the total is exact and independent of the lane grouping - so this
/// four-lane `i32` loop yields the **same** number, and LLVM can vectorize it
/// (`i8 * i8 -> i16` widened, `i32` accumulate) without the `std::simd`
/// intrinsics that `_mm_maddubs_epi16` would need.
///
/// The grouping is spelled out rather than left to the iterator sum: a plain
/// `map().sum()` over `i8` products lets the compiler widen after the fact and
/// keeps the loop scalar, which is the shape the profile showed.
pub fn i8_ip_residual(a: &[i8], b: &[i8]) -> i32 {
    const LANES: usize = 16;
    let n = a.len().min(b.len());
    let mut acc = [0i32; 4];
    let mut i = 0;
    while i + LANES <= n {
        for (lane, acc) in acc.iter_mut().enumerate() {
            // Four independent accumulators, `LANES / 4` products each: the
            // exact sum is the same and the dependency chain is 4x shorter.
            let at = i + lane * 4;
            let mut part = 0i32;
            for k in 0..4 {
                part += i32::from(a[at + k]) * i32::from(b[at + k]);
            }
            *acc += part;
        }
        i += LANES;
    }
    let mut sum = acc.iter().sum::<i32>();
    // `I8IPBF`: the tail the reference also does scalar.
    while i < n {
        sum += i32::from(a[i]) * i32::from(b[i]);
        i += 1;
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next_f32(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
            let bits = (self.0 >> 33) as u32;
            bits as f32 / u32::MAX as f32
        }
        fn vector(&mut self, dim: usize, lo: f32, hi: f32) -> Vec<f32> {
            (0..dim).map(|_| lo + (hi - lo) * self.next_f32()).collect()
        }
    }

    fn plain_l2(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
    }

    /// Port of `test_lvq_ut.cpp`: the reconstruction error after the batch
    /// mean and after a recenter is below 2% per element.
    #[test]
    fn reconstruction_error_is_below_two_percent() {
        let dim = 16;
        let mut rng = Rng(42);
        let vectors: Vec<Vec<f32>> = (0..32).map(|_| rng.vector(dim, 100.0, 200.0)).collect();
        let mut store = LvqStore::new(dim, Metric::L2, false);
        store.build(&vectors);
        for (i, v) in vectors.iter().enumerate() {
            let d = store.decompress(i).unwrap();
            for (j, x) in v.iter().enumerate() {
                let error = ((d[j] as f32 - x) / x).abs();
                assert!(error <= 2e-2, "vector {i} dim {j}: error {error}");
            }
        }
    }

    /// `AddVec` after a first batch keeps the mean; the distances stay within
    /// the quantization error of the plain metric.
    #[test]
    fn distances_track_the_plain_metric() {
        let dim = 16;
        let mut rng = Rng(7);
        let vectors: Vec<Vec<f32>> = (0..64).map(|_| rng.vector(dim, -1.0, 1.0)).collect();
        let mut store = LvqStore::new(dim, Metric::L2, false);
        store.build(&vectors);
        for _ in 0..8 {
            let q = rng.vector(dim, -1.0, 1.0);
            let query = store.make_query(&q);
            for (i, v) in vectors.iter().enumerate() {
                let truth = plain_l2(&q, v);
                let estimate = store.distance(&query, i).unwrap();
                let tolerance = truth.abs().max(1.0) * 0.15;
                assert!(
                    (estimate - truth).abs() <= tolerance,
                    "i {i}: truth {truth}, estimate {estimate}"
                );
            }
        }
    }

    /// The borrowed two-record distance is exactly the `LvqQuery` one.
    ///
    /// `distance_between` is the graph builder's path: it is what
    /// `Store::distance_nodes` calls for every candidate comparison, so it must
    /// agree with `distance(&query_at(a))` without materialising the clone
    /// that `query_at` performs.
    #[test]
    fn borrowed_distance_matches_the_query_path() {
        let dim = 16;
        let mut rng = Rng(21);
        let vectors: Vec<Vec<f32>> = (0..32).map(|_| rng.vector(dim, -1.0, 1.0)).collect();
        for metric in [Metric::L2, Metric::Ip, Metric::Cos] {
            let mut store = LvqStore::new(dim, metric, metric == Metric::Cos);
            store.build(&vectors);
            for a in 0..store.len() {
                let q = store.query_at(a).expect("record a");
                for b in 0..store.len() {
                    let via_query = store.distance(&q, b).expect("record b");
                    let borrowed = store.distance_between(&q.data, &store.record(b).unwrap());
                    assert_eq!(via_query, borrowed, "{metric:?} a={a} b={b}");
                }
            }
        }
    }

    /// Save/load round-trips the meta and every record.
    #[test]
    fn save_and_load_roundtrip() {
        let dim = 8;
        let mut rng = Rng(3);
        let vectors: Vec<Vec<f32>> = (0..10).map(|_| rng.vector(dim, 1.0, 2.0)).collect();
        for metric in [Metric::L2, Metric::Ip, Metric::Cos] {
            let mut store = LvqStore::new(dim, metric, metric == Metric::Cos);
            store.build(&vectors);
            let bytes = store.to_bytes();
            let (loaded, at) =
                LvqStore::from_bytes(metric, store.normalize, &bytes, store.len()).unwrap();
            assert_eq!(at, bytes.len());
            assert_eq!(loaded.mean, store.mean);
            assert_eq!(loaded.data, store.data);
            // The cosine global cache is stored as `pair<float,float>` in C++.
            if let (Some(a), Some(b)) = (store.global, loaded.global) {
                assert!((a.0 - b.0).abs() <= a.0.abs().max(1.0) * 1e-6);
                assert!((a.1 - b.1).abs() <= a.1.abs().max(1.0) * 1e-6);
            }
        }
    }

    /// C++: `DistFuncTest.test1` (`test_dist_func_ut.cpp`): the i8 residual
    /// kernel equals the scalar int32 dot product over random vectors.
    #[test]
    fn i8_ip_residual_matches_the_scalar_dot_product() {
        let dim = 200;
        let vec_n = 10_000;
        let mut rng = Rng(11);
        for _ in 0..vec_n {
            let mut a = vec![0i8; dim];
            let mut b = vec![0i8; dim];
            for j in 0..dim {
                // `uniform_int_distribution<int8_t>(-128, 127)`.
                a[j] = ((rng.next_f32() * 255.0).floor() as i32 - 128) as i8;
                b[j] = ((rng.next_f32() * 255.0).floor() as i32 - 128) as i8;
            }
            let mut expected = 0i64;
            for j in 0..dim {
                expected += i64::from(a[j]) * i64::from(b[j]);
            }
            assert_eq!(i64::from(i8_ip_residual(&a, &b)), expected);
        }
    }

    /// Loading with a backing mapping must produce the same distances as
    /// loading without one.
    ///
    /// The mapped path is what a loaded index takes and the copying path is what
    /// the builder and every existing test take, so a divergence between them
    /// would mean the codes read out of a chunk are not the codes written into
    /// it - silently, since a distance is still a number.
    #[test]
    fn mapped_codes_match_the_copied_ones() {
        let dim = 64;
        let vec_n = 256;
        let mut rng = Rng(7);
        let stored: Vec<Vec<f32>> = (0..vec_n).map(|_| rng.vector(dim, 0.0, 1.0)).collect();
        let mut built = LvqStore::new(dim, Metric::L2, false);
        for v in &stored {
            built.compress(v);
        }
        let meta = built.meta_bytes();
        let records = built.records_bytes();

        let copied =
            LvqStore::from_parts(Metric::L2, false, &meta, &records, None, vec_n).expect("copied");
        let owner: std::sync::Arc<dyn AsRef<[u8]> + Send + Sync> =
            std::sync::Arc::new(records.clone());
        // The window must point into the same bytes the mapping holds, so the
        // test hands `from_parts` the `Arc`'s own buffer.
        let mapped = LvqStore::from_parts(
            Metric::L2,
            false,
            &meta,
            owner.as_ref().as_ref(),
            Some(std::sync::Arc::clone(&owner)),
            vec_n,
        )
        .expect("mapped");

        assert_eq!(copied.len(), mapped.len());
        for i in 0..vec_n {
            assert_eq!(
                copied.record(i).unwrap().code.as_slice(),
                mapped.record(i).unwrap().code.as_slice(),
                "vector {i} codes differ"
            );
            // All four fields, not just the two that happen to be compared by
            // the distance: `local_0`/`local_1` are at offsets 8 and 12, which
            // is exactly where a wrong record stride would show up.
            assert_eq!(
                *copied.record(i).unwrap().header,
                *mapped.record(i).unwrap().header,
                "vector {i} header differs"
            );
            let q = mapped.make_query(&stored[i]);
            assert_eq!(
                copied.distance(&q, i).unwrap(),
                mapped.distance(&q, i).unwrap(),
                "vector {i} distance differs"
            );
        }
    }

    /// Reconstructs the LVQ vector a compressed record represents.
    fn decode(data: &LvqData, mean: &[f64]) -> Vec<f32> {
        data.code
            .as_slice()
            .iter()
            .enumerate()
            .map(|(j, c)| data.header.scale * f32::from(*c) + data.header.bias + mean[j] as f32)
            .collect()
    }

    /// C++: `DistFuncTest.test2` (`test_dist_func_ut.cpp`): the LVQ L2 distance
    /// equals the plain L2 of the two decompressed vectors (relative error
    /// below 1e-5).
    #[test]
    fn lvq_l2_distance_matches_decompressed_vectors() {
        let dim = 200;
        let vec_n = 8192;
        let mut rng = Rng(5);
        let stored: Vec<Vec<f32>> = (0..vec_n).map(|_| rng.vector(dim, 0.0, 1.0)).collect();
        let queries: Vec<Vec<f32>> = (0..vec_n).map(|_| rng.vector(dim, 0.0, 1.0)).collect();
        let mut store = LvqStore::new(dim, Metric::L2, false);
        for v in &stored {
            store.compress(v);
        }
        for (i, query) in queries.iter().enumerate() {
            let q = store.make_query(query);
            let dist1 = store.distance(&q, i).unwrap();
            let qv1 = decode(q.data(), &store.mean);
            let qv2 = decode(&store.record(i).unwrap(), &store.mean);
            let dist2 = plain_l2(&qv1, &qv2);
            let err = ((dist1 - dist2) / dist1).abs();
            assert!(
                err < 1e-5,
                "vector {i}: dist1 {dist1}, dist2 {dist2}, err {err}"
            );
        }
    }

    /// C++: `DistFuncTest2.test2` (`test_dist_func2_ut.cpp`): the LVQ inner
    /// product distance equals `-dot` of the decompressed vectors.
    #[test]
    fn lvq_ip_distance_matches_decompressed_vectors() {
        let dim = 128;
        let vec_n = 8192;
        let mut rng = Rng(6);
        let stored: Vec<Vec<f32>> = (0..vec_n).map(|_| rng.vector(dim, 0.0, 1.0)).collect();
        let queries: Vec<Vec<f32>> = (0..vec_n).map(|_| rng.vector(dim, 0.0, 1.0)).collect();
        let mut store = LvqStore::new(dim, Metric::Ip, false);
        for v in &stored {
            store.compress(v);
        }
        for (i, query) in queries.iter().enumerate() {
            let q = store.make_query(query);
            let dist1 = store.distance(&q, i).unwrap();
            let qv1 = decode(q.data(), &store.mean);
            let qv2 = decode(&store.record(i).unwrap(), &store.mean);
            let dist2: f32 = -qv1.iter().zip(&qv2).map(|(a, b)| a * b).sum::<f32>();
            let err = ((dist1 - dist2) / dist1).abs();
            assert!(
                err < 1e-5,
                "vector {i}: dist1 {dist1}, dist2 {dist2}, err {err}"
            );
        }
    }
}
