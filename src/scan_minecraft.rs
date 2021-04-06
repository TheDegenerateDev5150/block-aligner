#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use crate::avx2::*;

#[cfg(target_arch = "wasm32")]
use crate::simd128::*;

use crate::scores::*;

use std::{alloc, cmp, ptr, i16};
use std::cmp::Ordering;
use std::marker::PhantomData;

const NULL: u8 = b'A' + 26u8; // this null byte value works for both amino acids and nucleotides

#[inline(always)]
fn convert_char(c: u8, nuc: bool) -> u8 {
    debug_assert!(c >= b'A' && c <= NULL);
    if nuc { c } else { c - b'A' }
}

#[inline(always)]
fn clamp(x: i32) -> i16 {
    cmp::min(cmp::max(x, i16::MIN as i32), i16::MAX as i32) as i16
}

#[inline(always)]
fn div_ceil(n: usize, d: usize) -> usize {
    (n + d - 1) / d
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub struct AlignResult {
    pub score: i32,
    pub query_idx: usize,
    pub reference_idx: usize
}

#[derive(Copy, Clone, PartialEq, Debug)]
enum Direction {
    Right,
    Down,
    Diagonal
}

pub struct Trace {
    trace: Vec<u32>,
    shift_dir: Vec<u32>,
    idx: usize
}

impl Trace {
    #[inline(always)]
    pub fn new(query_len: usize, reference_len: usize) -> Self {
        let len = query_len + reference_len;
        Self {
            trace: vec![0; div_ceil(len, 16)],
            shift_dir: vec![0; div_ceil(div_ceil(len, L), 16)],
            idx: 0
        }
    }

    #[inline(always)]
    pub fn add(&mut self, t: u32) {
        unsafe { *self.trace.get_unchecked_mut(self.idx) = t; }
        self.idx += 1;
    }

    #[inline(always)]
    pub fn dir(&mut self, d: u32) {
        let i = self.idx / L;
        unsafe {
            *self.shift_dir.get_unchecked_mut(i / 16) |= d << (i % 16);
        }
    }
}

// Notes:
//
// BLOSUM62 matrix max = 11, min = -4; gap open = -11 (includes extension), gap extend = -1
//
// R[i][j] = max(R[i - 1][j] + gap_extend, D[i - 1][j] + gap_open)
// C[i][j] = max(C[i][j - 1] + gap_extend, D[i][j - 1] + gap_open)
// D[i][j] = max(D[i - 1][j - 1] + matrix[query[i]][reference[j]], R[i][j], C[i][j])
//
// indexing (we want to calculate D11):
//      x0   x1
//    +--------
// 0x | 00   01
// 1x | 10   11
//
// note that 'x' represents any bit
//
// Each band is made up of strided SIMD vectors of length 8 or 16 16-bit integers.

#[allow(non_snake_case)]
pub struct ScanAligner<'a, P: ScoreParams, M: 'a + Matrix, const K: usize, const TRACE: bool, const X_DROP: bool> {
    trace: Trace,
    query: &'a [u8],
    matrix: &'a M,
    _phantom: PhantomData<P>
}

impl<'a, P: ScoreParams, M: 'a + Matrix, const K: usize, const TRACE: bool, const X_DROP: bool> ScanAligner<'a, P, M, { K }, { TRACE }, { X_DROP }> {
    const EVEN_BITS: u32 = 0x55555555u32;

    #[cfg_attr(any(target_arch = "x86", target_arch = "x86_64"), target_feature(enable = "avx2"))]
    #[cfg_attr(target_arch = "wasm32", target_feature(enable = "simd128"))]
    #[allow(non_snake_case)]
    pub unsafe fn new(query: &'a [u8], matrix: &'a M) -> Self {
        assert!(P::GAP_OPEN <= P::GAP_EXTEND);

        // Initialize DP columns (the first band)
        // Not extremely optimized, since it only runs once
        {
            for i in 0..Self::CEIL_K {
                let buf_idx = (i % Self::STRIDE) * L + i / Self::STRIDE;
                debug_assert!(buf_idx < Self::CEIL_K);

                if i <= query.len() {
                    ptr::write(query_buf_ptr.add(halfsimd_get_idx(buf_idx)), convert_char(if i > 0 {
                        *query.get_unchecked(i - 1) } else { NULL }, M::NUC));

                    let val = if i > 0 {
                        (P::GAP_OPEN as i32) + ((i as i32) - 1) * (P::GAP_EXTEND as i32)
                    } else {
                        0
                    };

                    ptr::write(delta_Dx0_ptr.add(buf_idx), val as i16);
                } else {
                    ptr::write(query_buf_ptr.add(halfsimd_get_idx(buf_idx)), convert_char(NULL, M::NUC));
                    ptr::write(delta_Dx0_ptr.add(buf_idx), i16::MIN);
                }

                ptr::write(delta_Cx0_ptr.add(buf_idx), i16::MIN);
            }
        }

        Self {
            trace: Trace::new(),
            query,
            matrix,
            _phantom: PhantomData
        }
    }

    // TODO: deal with trace when shifting down
    // TODO: count number of down/right shifts for profiling

    #[cfg_attr(any(target_arch = "x86", target_arch = "x86_64"), target_feature(enable = "avx2"))]
    #[cfg_attr(target_arch = "wasm32", target_feature(enable = "simd128"))]
    #[allow(non_snake_case)]
    #[inline]
    unsafe fn place_block_right(&mut self,
                                query: &[u8],
                                query_idx: usize,
                                reference: &[u8],
                                reference_idx: usize,
                                off: i32,
                                corner: i16,
                                mut D10: Simd,
                                mut C10: Simd,
                                D_buf: *mut i16,
                                R_buf: *mut i16,
                                neg_inf: Simd,
                                gap_open: Simd,
                                gap_extend: Simd,
                                gap_extend1234: Simd) -> (Simd, Simd, Simd, Simd) {
        let mut D00 = simd_sl_i16!(D10, simd_set1_i16(off), 1);
        let mut D_max = neg_inf;
        let mut D_argmax = simd_set1_i16(0);
        let mut curr_i = simd_set1_i16(0);

        if TRACE {
            self.trace.dir(0b01);
        }

        for i in 0..L {
            let matrix_ptr = self.matrix.as_ptr(convert_char(*reference.get_unchecked(reference_idx + i), M::NUC) as usize);
            let scores1 = halfsimd_load(matrix_ptr as *const HalfSimd);
            let scores2 = if M::NUC {
                halfsimd_set1_i8(0) // unused, should be optimized out
            } else {
                halfsimd_load((matrix_ptr as *const HalfSimd).add(1))
            };

            // efficiently lookup scores for each query character
            // TODO: query index -1
            let scores = if M::NUC {
                halfsimd_lookup1_i16(scores1, halfsimd_loadu(query.as_ptr().add(query_idx) as _))
            } else {
                halfsimd_lookup2_i16(scores1, scores2, halfsimd_loadu(query.as_ptr().add(query_idx) as _))
            };

            let mut D11 = simd_adds_i16(D00, scores);
            let C11 = simd_max_i16(simd_adds_i16(C10, gap_extend), simd_adds_i16(D10, gap_open));
            D11 = simd_max_i16(D11, C11);

            let trace_D_C = if TRACE {
                simd_movemask_i8(simd_cmpeq_i16(delta_D11, delta_C11))
            } else {
                0 // should be optimized out
            };

            let R11 = {
                let mut R = simd_sl_i16!(D11, neg_inf, 1);
                R = simd_prefix_scan_i16(R, gap_extend, gap_extend1234, neg_inf);
                simd_adds_i16(R, gap_open)
            };
            D11 = simd_max_i16(D11, R11);

            if TRACE {
                let trace_D_R = simd_movemask_i8(simd_cmpeq_i16(delta_D11, delta_R11));
                self.trace.add(((trace_D_R & Self::EVEN_BITS) << 1) | (trace_D_C & Self::EVEN_BITS));
            }

            if X_DROP {
                D_max = simd_max_i16(D_max, D11);
                let mask = simd_cmpeq_i16(D_max, D11);
                D_argmax = simd_blend_i8(D_argmax, curr_i, mask);
                curr_i = simd_adds_i16(curr_i, simd_set1_i16(1));
            }

            ptr::write(D_buf.add(i), simd_extract_i16::<{ L - 1 }>(D11));
            ptr::write(R_buf.add(i), simd_extract_i16::<{ L - 1 }>(R11));

            D00 = simd_sl_i16!(D11, neg_inf, 1);
            D10 = D11;
            C10 = C11;

            if !X_DROP && query_idx + L > query.len()
                && reference_idx + i >= reference.len() {
                break;
            }
        }

        (D10, C10, D_max, D_argmax)
    }

    #[cfg_attr(any(target_arch = "x86", target_arch = "x86_64"), target_feature(enable = "avx2"))]
    #[cfg_attr(target_arch = "wasm32", target_feature(enable = "simd128"))]
    #[allow(non_snake_case)]
    pub unsafe fn align(&mut self, reference: &[u8], x_drop: i32) {
        // some useful constant simd vectors
        let neg_inf = simd_set1_i16(i16::MIN);
        let gap_open = simd_set1_i16(P::GAP_OPEN as i16);
        let gap_extend = simd_set1_i16(P::GAP_EXTEND as i16);
        let gap_extend1234 = simd_set4_i16(
            (P::GAP_EXTEND as i16) * 4,
            (P::GAP_EXTEND as i16) * 3,
            (P::GAP_EXTEND as i16) * 2,
            (P::GAP_EXTEND as i16) * 1
        );

        let mut i = 0;
        let mut j = 0;
        let mut best_max = i32::MIN;
        let mut best_argmax_i = 0usize;
        let mut best_argmax_j = 0usize;
        let mut dir = Direction::Diagonal;
        let mut prev_dir = Direction::Diagonal;
        let mut off = 0i32;
        let mut corner1 = i16::MIN as i32;
        let mut corner2 = i16::MIN as i32;
        let mut D = simd_insert_i16::<{ L - 1 }>(neg_inf, 0i16);
        let mut C = neg_inf;
        let mut D_buf = A([i16::MIN; L]);
        let mut C_buf = A([i16::MIN; L]);

        loop {
            let (D_max, D_argmax) = match dir {
                Direction::Diagonal => {
                    let (D, C, D_max, D_argmax) = place_block_diagonal();
                },
                Direction::Right => {
                    let old_off = off;
                    off += simd_extract_i16::<0>(D);
                    let off_add = simd_set1_i16((old_off - off) as i16);
                    let corner = if prev_dir == Direction::Down { (corner1 - off) as i16 } else { i16::MIN };

                    let (new_D, new_C, D_max, D_argmax) = place_block_right(
                        self.query,
                        i,
                        reference,
                        j,
                        off,
                        corner,
                        simd_adds_i16(D, off_add),
                        simd_adds_i16(C, off_add),
                        D_buf.as_mut_ptr(),
                        C_buf.as_mut_ptr(),
                        neg_inf,
                        gap_open,
                        gap_extend,
                        gap_extend1234
                    );

                    D = new_D;
                    C = new_C;
                    (D_max, D_argmax)
                },
                Direction::Down => {

                }
            };

            if X_DROP {
                let max = simd_hmax_i16(D_max);

                if off + max < best_max - x_drop {
                    break;
                }

                if off + max > best_max {
                    let lane_idx = (simd_movemask_epi8(
                            simd_cmpeq_i16(D_max, simd_set1_i16(max))).trailing_zeros() / 2) as usize;
                    best_argmax_i = i + lane_idx;
                    best_argmax_j = j + simd_slow_extract_i16(D_argmax, lane_idx) as usize;
                    best_max = off + max;
                }
            }

            let right_max = simd_hmax_i16(D);
            let down_max = simd_hmax_i16(simd_load(D_buf.as_ptr() as _));
            prev_dir = dir;

            if i + L > query.len() && j + L > reference.len() {
                break;
            } else if j + L > reference.len() || down_max > right_max {
                i += L;
                dir = Direction::Down;
            } else if i + L > query.len() || right_max > down_max {
                j += L;
                dir = Direction::Right;
            } else if right_max == down_max && down_max == D_buf[L - 1] {
                i += L - 1;
                j += L - 1;
                dir = Direction::Diagonal;
            } else {
                // arbitrary
                j += L;
                dir = Direction::Right;
            }

            corner1 = corner2;
            corner2 = off + D_buf[L - 1];
        }

        if X_DROP {
            AlignResult {
                best_max,
                best_argmax_i,
                best_argmax_j
            }
        } else {
            AlignResult {
                off + simd_slow_extract_i16(D, query.len() - i) as i32,
                query.len(),
                reference.len()
            }
        }
    }

    /// Adaptive banded alignment.
    ///
    /// The x drop option indicates whether to terminate the alignment process early when
    /// the max score in the current band drops below the max score encountered so far. If
    /// x drop is not enabled, then the band will keep shifting until the end of the reference
    /// string is reached.
    ///
    /// Limitations:
    /// 1. Requires x86 AVX2 or WASM SIMD support.
    /// 2. The reference and the query can only contain uppercase alphabetical characters.
    /// 3. The actual size of the band is K + 1 rounded up to the next multiple of the
    ///    vector length of 16 (for x86 AVX2) or 8 (for WASM SIMD).
    #[cfg_attr(any(target_arch = "x86", target_arch = "x86_64"), target_feature(enable = "avx2"))]
    #[cfg_attr(target_arch = "wasm32", target_feature(enable = "simd128"))]
    #[allow(non_snake_case)]
    pub unsafe fn align(&mut self, reference: &[u8], x_drop: i32) {
        if X_DROP {
            assert!(x_drop >= 0);
        }

        // optional 32-bit traceback
        // 0b00 = up and left, 0b10 or 0b11 = up, 0b01 = left
        if TRACE {
            self.trace.resize(self.trace.len() + (reference.len() + 1) * Self::CEIL_K / L, Self::EVEN_BITS << 1);
        }

        let gap_open = simd_set1_i16(P::GAP_OPEN as i16);
        let gap_extend = simd_set1_i16(P::GAP_EXTEND as i16);
        let neg_inf = simd_set1_i16(i16::MIN);

        let stride_gap_scalar = (Self::STRIDE as i16) * (P::GAP_EXTEND as i16);
        let stride_gap = simd_set1_i16(stride_gap_scalar);
        let stride_gap1234 = simd_set4_i16(stride_gap_scalar * 4,
                                           stride_gap_scalar * 3,
                                           stride_gap_scalar * 2,
                                           stride_gap_scalar * 1);

        // values that are "shared" between the code for shifting down and shifting right
        let mut delta_D00 = simd_sl_i16!(simd_load({
            // get last stride vector and shift it
            let idx = (self.ring_buf_idx + Self::STRIDE - 1) % Self::STRIDE;
            self.delta_Dx0_ptr.add(idx)
        }), neg_inf, 1);
        let mut abs_R_band = i32::MIN;
        let mut abs_D_band = i16::MIN as i32;
        let mut j = 0usize;

        'outer: while j < reference.len() {
            match self.shift_dir {
                Direction::Down(shift_iter) => {
                    // fixed number of shift iterations because newly calculated D values are
                    // decreasing due to gap penalties
                    for _i in 0..shift_iter {
                        // Don't go past the end of the query
                        if self.shift_idx() >= self.query.len() {
                            self.shift_dir = Direction::Right;
                            continue 'outer;
                        }

                        let shift_vec_idx = self.ring_buf_idx % Self::STRIDE;
                        debug_assert!(shift_vec_idx < Self::CEIL_K / L);

                        // Update ring buffers to slide current band down
                        // the benefit of using ring buffers is apparent here: shifting down
                        // only requires shifting one simd vector and incrementing an index
                        let shift_D_ptr = self.delta_Dx0_ptr.add(shift_vec_idx);
                        let shift_query_ptr = self.query_buf_ptr.add(shift_vec_idx);
                        let shift_C_ptr = self.delta_Cx0_ptr.add(shift_vec_idx);

                        let c = if self.query_idx() < self.query.len() {
                            *self.query.get_unchecked(self.query_idx())
                        } else {
                            NULL
                        };
                        let query_insert = halfsimd_set1_i8(convert_char(c, M::NUC) as i8);

                        // abs_R_band is only used for the first iteration
                        // it already has the gap extend cost included
                        abs_D_band = cmp::max(abs_D_band + P::GAP_OPEN as i32, abs_R_band);
                        abs_R_band = i32::MIN;

                        let delta_Dx0_insert = simd_set1_i16(clamp(abs_D_band - self.abs_A00));

                        // Now shift in new values for each band
                        halfsimd_store(shift_query_ptr, halfsimd_sr_i8!(query_insert, halfsimd_load(shift_query_ptr), 1));
                        delta_D00 = simd_load(shift_D_ptr);
                        simd_store(shift_D_ptr, simd_sr_i16!(delta_Dx0_insert, delta_D00, 1));
                        simd_store(shift_C_ptr, simd_sr_i16!(neg_inf, simd_load(shift_C_ptr), 1));

                        self.ring_buf_idx += 1;
                    }

                    self.shift_dir = Direction::Right;
                },
                Direction::Right => {
                    // Load scores for the current reference character
                    let matrix_ptr = self.matrix.as_ptr(convert_char(*reference.get_unchecked(j), M::NUC) as usize);
                    let scores1 = halfsimd_load(matrix_ptr as *const HalfSimd);
                    let scores2 = if M::NUC {
                        halfsimd_set1_i8(0) // unused, should be optimized out
                    } else {
                        halfsimd_load((matrix_ptr as *const HalfSimd).add(1))
                    };

                    // Vector for prefix scan calculations
                    let mut delta_R_max = neg_inf;
                    // add the first D value of the previous band to the absolute A value of the
                    // previous band to get the absolute A value of the current band
                    let abs_band = self.abs_A00.saturating_add({
                        let ptr = self.delta_Dx0_ptr.add(self.ring_buf_idx % Self::STRIDE);
                        simd_extract_i16::<0>(simd_load(ptr)) as i32
                    });
                    // need to offset the values from the previous band
                    let abs_offset = simd_set1_i16(clamp(self.abs_A00 - abs_band));

                    delta_D00 = simd_adds_i16(delta_D00, abs_offset);

                    // Begin initial pass
                    {
                        
                    }
                    // End initial pass

                    // Begin prefix scan
                    {
                        let prev_delta_R_max_last = simd_extract_i16::<{ L - 1 }>(delta_R_max) as i32;

                        delta_R_max = simd_sl_i16!(delta_R_max, neg_inf, 1);
                        delta_R_max = simd_prefix_scan_i16(delta_R_max, stride_gap, stride_gap1234, neg_inf);

                        let curr_delta_R_max_last = simd_extract_i16::<{ L - 1 }>(simd_adds_i16(delta_R_max, stride_gap)) as i32;
                        // this is the absolute R value for the last cell of the band, plus
                        // the gap open cost
                        abs_R_band = abs_band.saturating_add(
                            cmp::max(prev_delta_R_max_last, curr_delta_R_max_last) + (P::GAP_OPEN as i32));
                    }
                    // End prefix scan

                    let mut delta_D_max = neg_inf;
                    let mut delta_D_argmax = simd_set1_i16(0);

                    // Begin final pass
                    {
                        let mut delta_R01 = simd_adds_i16(simd_subs_i16(delta_R_max, gap_extend), gap_open);
                        let mut delta_D01 = neg_inf;
                        let mut curr_i = simd_set1_i16(0);

                        for i in 0..Self::STRIDE {
                            let idx = (self.ring_buf_idx + i) % Self::STRIDE;
                            debug_assert!(idx < Self::CEIL_K / L);

                            let delta_R11 = simd_max_i16(
                                simd_adds_i16(delta_R01, gap_extend), simd_adds_i16(delta_D01, gap_open));
                            let mut delta_D11 = simd_load(self.delta_Dx0_ptr.add(idx));
                            delta_D11 = simd_max_i16(delta_D11, delta_R11);

                            if TRACE {
                                let trace_idx = (Self::CEIL_K / L) * (j + 1) + i;
                                debug_assert!(trace_idx < self.trace.len());
                                let prev_trace = *self.trace.get_unchecked(trace_idx);
                                let curr_trace = simd_movemask_i8(simd_cmpeq_i16(delta_R11, delta_D11));
                                *self.trace.get_unchecked_mut(trace_idx) =
                                    (prev_trace & Self::EVEN_BITS) | ((curr_trace & Self::EVEN_BITS) << 1);
                            }

                            // consistently update the max D value for each stride vector
                            delta_D_max = simd_max_i16(delta_D_max, delta_D11);
                            let mask = simd_cmpeq_i16(delta_D_max, delta_D11);
                            delta_D_argmax = simd_blend_i8(delta_D_argmax, curr_i, mask);
                            curr_i = simd_adds_i16(curr_i, simd_set1_i16(1));

                            simd_store(self.delta_Dx0_ptr.add(idx), delta_D11);

                            delta_D01 = delta_D11;
                            delta_R01 = delta_R11;
                        }

                        // this is the absolute D value for the last cell of the band
                        abs_D_band = abs_band.saturating_add(simd_extract_i16::<{ L - 1 }>(delta_D01) as i32);

                        // updating delta_D00 is important if the band shifts right
                        delta_D00 = simd_sl_i16!(delta_D01, neg_inf, 1);
                    }
                    // End final pass

                    let (max, lane_idx) = simd_hargmax_i16(delta_D_max);
                    let max = (max as i32).saturating_add(abs_band);
                    // "slow" because it allows an index only known at run time
                    let stride_idx = simd_slow_extract_i16(delta_D_argmax, lane_idx) as u16 as usize;
                    let argmax = stride_idx + lane_idx * Self::STRIDE;

                    self.abs_A00 = abs_band;

                    if X_DROP && max < self.best_max - x_drop {
                        break;
                    }

                    // if not x drop, then keep track of values only for the current band
                    let cond = !X_DROP || max > self.best_max;
                    self.best_argmax_i = if cond { argmax + self.shift_idx() } else { self.best_argmax_i };
                    self.best_argmax_j = if cond { j + self.ref_idx + 1 } else { self.best_argmax_j };
                    self.best_max = if cond { max } else { self.best_max };

                    // high threshold for starting to shift down, to prevent switching back and
                    // forth between down and right all time
                    self.shift_dir = if argmax > Self::CEIL_K * 5 / 8 {
                        Direction::Down(argmax - Self::CEIL_K / 2)
                    } else {
                        Direction::Right
                    };

                    j += 1;
                }
            }
        }

        self.ref_idx += reference.len();
    }

    pub fn trace(&self) -> &Trace {
        assert!(TRACE);
        &self.trace
    }
}

#[cfg(test)]
mod tests {
    use crate::scores::*;

    use super::*;

    #[test]
    fn test_scan_align() {
        type TestParams = Params<-11, -1, 1024>;

        unsafe {
            let r = b"AAAA";
            let q = b"AARA";
            let mut a = ScanAligner::<TestParams, _, 2, false, false>::new(q, &BLOSUM62);
            a.align(r, 0);
            assert_eq!(a.score(), 11);

            let r = b"AAAA";
            let q = b"AARA";
            let mut a = ScanAligner::<TestParams, _, 6, false, false>::new(q, &BLOSUM62);
            a.align(r, 0);
            assert_eq!(a.score(), 11);

            let r = b"AAAA";
            let q = b"AAAA";
            let mut a = ScanAligner::<TestParams, _, 2, false, false>::new(q, &BLOSUM62);
            a.align(r, 0);
            assert_eq!(a.score(), 16);

            let r = b"AAAA";
            let q = b"AARA";
            let mut a = ScanAligner::<TestParams, _, 1, false, false>::new(q, &BLOSUM62);
            a.align(r, 0);
            assert_eq!(a.score(), 11);

            let r = b"AAAA";
            let q = b"RRRR";
            let mut a = ScanAligner::<TestParams, _, 8, false, false>::new(q, &BLOSUM62);
            a.align(r, 0);
            assert_eq!(a.score(), -4);

            let r = b"AAAA";
            let q = b"AAA";
            let mut a = ScanAligner::<TestParams, _, 2, false, false>::new(q, &BLOSUM62);
            a.align(r, 0);
            assert_eq!(a.score(), 1);

            type TestParams2 = Params<-1, -1, 2048>;

            let r = b"AAAN";
            let q = b"ATAA";
            let mut a = ScanAligner::<TestParams2, _, 4, false, false>::new(q, &NW1);
            a.align(r, 0);
            assert_eq!(a.score(), 1);

            let r = b"AAAA";
            let q = b"C";
            let mut a = ScanAligner::<TestParams2, _, 8, false, false>::new(q, &NW1);
            a.align(r, 0);
            assert_eq!(a.score(), -4);
            let mut a = ScanAligner::<TestParams2, _, 8, false, false>::new(r, &NW1);
            a.align(q, 0);
            assert_eq!(a.score(), -1);
        }
    }

    #[test]
    fn test_x_drop() {
        type TestParams = Params<-11, -1, 1024>;

        unsafe {
            let r = b"AAARRA";
            let q = b"AAAAAA";
            let mut a = ScanAligner::<TestParams, _, 3, false, true>::new(q, &BLOSUM62);
            a.align(r, 1);
            assert_eq!(a.score(), 12);
            assert_eq!(a.end_idx(), EndIndex { query_idx: 3, ref_idx: 3 });

            let r = b"AAARRA";
            let q = b"AAAAAA";
            let mut a = ScanAligner::<TestParams, _, 20, false, true>::new(q, &BLOSUM62);
            a.align(r, 1);
            assert_eq!(a.score(), 12);
            assert_eq!(a.end_idx(), EndIndex { query_idx: 3, ref_idx: 3 });
        }
    }
}
