// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Bounds-checked views over raw 2-D pixel/field buffers.
//!
//! Several hand-rolled raster paths (whiteboard ink, teleport's scry frame) used to pass a bare
//! `(ptr, w, h)` triple down a call chain of `unsafe fn`s that trusted the CALLER'S clamps
//! entirely — no internal validation. If a caller ever held a stale `w`/`h`
//! relative to the buffer the pointer actually points at (e.g. a DPI change resizing the canvas
//! while a closure still holds the old dimensions), the index math silently walks off the
//! allocation: heap corruption, not a panic.
//!
//! `RasterBuf`/`RasterView` fix this at the type level: the dimensions travel WITH the pointer,
//! constructed ONCE at the buffer-acquisition site (where `w`/`h` are authoritative — right where
//! the DIB/Vec is created or locked). Every raster function downstream takes `&mut RasterBuf`
//! instead of a loose triple, so a stale-dimensions mismatch is unrepresentable: there is no
//! second `w`/`h` for it to disagree with. Accessors bounds-check by default (`get`/`put`, clamp-
//! or-skip — a skipped pixel is cosmetically harmless); `row_mut` gives a hot inner loop ONE bounds
//! check for a whole scanline; `get_unchecked`/`put_unchecked` are available for loops that have
//! already computed a range clamped against `self.w()`/`self.h()` (the same struct's own fields,
//! not a separately-carried value), with a `debug_assert!` standing in for the check in release.

use std::marker::PhantomData;

/// A bounds-checked, mutable view over a `w * h` buffer of `T`, top-down, row-major.
pub struct RasterBuf<'a, T> {
    px: *mut T,
    w: i32,
    h: i32,
    _marker: PhantomData<&'a mut T>,
}

/// A bounds-checked, read-only view over a `w * h` buffer of `T` — for "snapshot"/"base" style
/// arguments that sit alongside a mutable target without aliasing it. `Copy`/`Clone`: it's just a
/// pointer + two dimensions, cheap to pass by value down a call chain.
#[derive(Clone, Copy)]
pub struct RasterView<'a, T> {
    px: *const T,
    w: i32,
    h: i32,
    _marker: PhantomData<&'a T>,
}

/// The canvas pixel buffer: premultiplied BGRA/ARGB `u32`s, top-down.
pub type PixelBuf<'a> = RasterBuf<'a, u32>;
/// A read-only pixel snapshot.
pub type PixelView<'a> = RasterView<'a, u32>;
/// A scalar field buffer (e.g. the whiteboard's per-pixel min-distance / arc-length fields).
pub type FieldBuf<'a> = RasterBuf<'a, f32>;
/// A read-only scalar field view.
pub type FieldView<'a> = RasterView<'a, f32>;

#[inline]
fn area(w: i32, h: i32) -> usize {
    if w <= 0 || h <= 0 {
        0
    } else {
        w as usize * h as usize
    }
}

#[inline]
fn in_bounds(w: i32, h: i32, x: i32, y: i32) -> bool {
    x >= 0 && y >= 0 && x < w && y < h
}

#[inline]
fn idx(w: i32, x: i32, y: i32) -> usize {
    (y as isize * w as isize + x as isize) as usize
}

impl<'a, T: Copy> RasterBuf<'a, T> {
    /// Build a checked view from a borrowed slice — validates `slice.len() == w * h` (a
    /// non-positive `w`/`h` yields an always-empty, always-clamped buffer), so a mismatched
    /// slice/dimension pair can never be constructed in the first place. This is the
    /// "stale-dimensions unrepresentable-by-construction" guarantee: once built, `w`/`h` are the
    /// dimensions that produced this exact pointer, nothing else.
    #[allow(dead_code)] // part of the seam's complete surface; exercised by the raster tests today
    pub fn new(slice: &'a mut [T], w: i32, h: i32) -> Self {
        let expect = area(w, h);
        assert_eq!(
            slice.len(),
            expect,
            "RasterBuf::new: slice len {} != w*h {} ({w}x{h})",
            slice.len(),
            expect
        );
        RasterBuf {
            px: slice.as_mut_ptr(),
            w,
            h,
            _marker: PhantomData,
        }
    }

    /// Wrap a raw pointer (a DIB/Win32 backing store) whose extent the caller has verified
    /// out-of-band, at the SAME site that knows the authoritative `w`/`h` (e.g. right after
    /// `CreateDIBSection`/`surf.bits()`). This is the one place the unchecked `(pointer, w, h)`
    /// triple is allowed to exist — everything downstream takes `&mut RasterBuf` and never sees
    /// the raw pointer again.
    ///
    /// # Safety
    /// `px` must be valid for reads and writes of `w * h` contiguous `T`s for the lifetime `'a`.
    pub unsafe fn from_raw_parts(px: *mut T, w: i32, h: i32) -> Self {
        RasterBuf {
            px,
            w,
            h,
            _marker: PhantomData,
        }
    }

    #[inline]
    pub fn w(&self) -> i32 {
        self.w
    }
    #[inline]
    pub fn h(&self) -> i32 {
        self.h
    }

    /// Bounds-checked read; `None` if `(x, y)` is off-buffer.
    #[inline]
    #[allow(dead_code)] // part of the seam's complete surface; exercised by the raster tests today
    pub fn get(&self, x: i32, y: i32) -> Option<T> {
        if !in_bounds(self.w, self.h, x, y) {
            return None;
        }
        Some(unsafe { *self.px.add(idx(self.w, x, y)) })
    }

    /// Bounds-checked write; a no-op (silently skipped) if `(x, y)` is off-buffer.
    #[inline]
    pub fn put(&mut self, x: i32, y: i32, v: T) {
        if !in_bounds(self.w, self.h, x, y) {
            return;
        }
        unsafe { *self.px.add(idx(self.w, x, y)) = v };
    }

    /// The full scanline `y`, ONE bounds check for the whole row — for hot loops that walk a
    /// contiguous run instead of scattered points. `None` if `y` is off-buffer.
    #[inline]
    pub fn row_mut(&mut self, y: i32) -> Option<&mut [T]> {
        if y < 0 || y >= self.h || self.w <= 0 {
            return None;
        }
        Some(unsafe { std::slice::from_raw_parts_mut(self.px.add(idx(self.w, 0, y)), self.w as usize) })
    }

    /// The inclusive `[x0, x1]` sub-slice of row `y`, clamped to the buffer — matches the raster
    /// functions' existing `x0.clamp(0, w-1)..=x1.clamp(0, w-1)` style but done once, safely,
    /// against `self.w()` (never a separately-carried value). `None` if the row or range is fully
    /// off-buffer.
    #[inline]
    pub fn row_range_mut(&mut self, y: i32, x0: i32, x1: i32) -> Option<&mut [T]> {
        let w = self.w;
        let row = self.row_mut(y)?;
        if x1 < 0 || x0 >= w || x1 < x0 {
            return None;
        }
        let cx0 = x0.max(0) as usize;
        let cx1 = (x1.min(w - 1)) as usize;
        let len = row.len();
        if cx0 > cx1 || cx0 >= len {
            return None;
        }
        Some(&mut row[cx0..=cx1.min(len - 1)])
    }

    /// Unchecked read for a loop that has already clamped `(x, y)` against `self.w()`/`self.h()`
    /// (the SAME struct the check would use — there is no separate dimension for it to disagree
    /// with). `debug_assert!`s the invariant; release builds trust the caller, matching the
    /// original raster functions' performance.
    ///
    /// # Safety
    /// Caller must ensure `0 <= x < self.w()` and `0 <= y < self.h()`.
    #[inline]
    pub unsafe fn get_unchecked(&self, x: i32, y: i32) -> T {
        debug_assert!(
            in_bounds(self.w, self.h, x, y),
            "RasterBuf::get_unchecked out of bounds ({x},{y}) in {}x{}",
            self.w,
            self.h
        );
        unsafe { *self.px.add(idx(self.w, x, y)) }
    }

    /// Unchecked write counterpart to [`get_unchecked`](Self::get_unchecked).
    ///
    /// # Safety
    /// Caller must ensure `0 <= x < self.w()` and `0 <= y < self.h()`.
    #[inline]
    pub unsafe fn put_unchecked(&mut self, x: i32, y: i32, v: T) {
        debug_assert!(
            in_bounds(self.w, self.h, x, y),
            "RasterBuf::put_unchecked out of bounds ({x},{y}) in {}x{}",
            self.w,
            self.h
        );
        unsafe { *self.px.add(idx(self.w, x, y)) = v };
    }

    /// A read-only view over the same memory (for passing to a helper that wants a "base"/
    /// snapshot argument without borrowing `&mut self` away).
    pub fn as_view(&self) -> RasterView<'_, T> {
        RasterView {
            px: self.px.cast_const(),
            w: self.w,
            h: self.h,
            _marker: PhantomData,
        }
    }
}

impl<'a, T: Copy> RasterView<'a, T> {
    /// Build a checked read-only view from a borrowed slice — same `len == w*h` validation as
    /// [`RasterBuf::new`].
    #[allow(dead_code)] // part of the seam's complete surface; exercised by the raster tests today
    pub fn new(slice: &'a [T], w: i32, h: i32) -> Self {
        let expect = area(w, h);
        assert_eq!(
            slice.len(),
            expect,
            "RasterView::new: slice len {} != w*h {} ({w}x{h})",
            slice.len(),
            expect
        );
        RasterView {
            px: slice.as_ptr(),
            w,
            h,
            _marker: PhantomData,
        }
    }

    /// Wrap a raw const pointer whose extent the caller has verified out-of-band.
    ///
    /// # Safety
    /// `px` must be valid for reads of `w * h` contiguous `T`s for the lifetime `'a`.
    pub unsafe fn from_raw_parts(px: *const T, w: i32, h: i32) -> Self {
        RasterView {
            px,
            w,
            h,
            _marker: PhantomData,
        }
    }

    #[inline]
    #[allow(dead_code)] // part of the seam's complete surface; exercised by the raster tests today
    pub fn w(&self) -> i32 {
        self.w
    }
    #[inline]
    #[allow(dead_code)] // part of the seam's complete surface; exercised by the raster tests today
    pub fn h(&self) -> i32 {
        self.h
    }

    #[inline]
    pub fn get(&self, x: i32, y: i32) -> Option<T> {
        if !in_bounds(self.w, self.h, x, y) {
            return None;
        }
        Some(unsafe { *self.px.add(idx(self.w, x, y)) })
    }

    /// # Safety
    /// Caller must ensure `0 <= x < self.w()` and `0 <= y < self.h()`.
    #[inline]
    pub unsafe fn get_unchecked(&self, x: i32, y: i32) -> T {
        debug_assert!(
            in_bounds(self.w, self.h, x, y),
            "RasterView::get_unchecked out of bounds ({x},{y}) in {}x{}",
            self.w,
            self.h
        );
        unsafe { *self.px.add(idx(self.w, x, y)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A guard-banded backing buffer: `w*h` logical words followed by `GUARD` sentinel words.
    /// Any raster op that writes past the logical region corrupts the sentinels. `whiteboard.rs`'s
    /// own adversarial tests build a lightweight equivalent locally (same pattern, since a
    /// private `#[cfg(test)] mod` doesn't cross module boundaries) — this is the `raster`-level
    /// copy, exercising `PixelBuf` itself.
    struct Guarded {
        buf: Vec<u32>,
        w: i32,
        h: i32,
    }
    const GUARD: usize = 64;
    const SENTINEL: u32 = 0xDEAD_BEEF;

    impl Guarded {
        pub fn new(w: i32, h: i32) -> Self {
            let n = area(w, h);
            let mut buf = vec![0u32; n + GUARD];
            for s in &mut buf[n..] {
                *s = SENTINEL;
            }
            Guarded { buf, w, h }
        }
        pub fn pixel_buf(&mut self) -> PixelBuf<'_> {
            let n = area(self.w, self.h);
            PixelBuf::new(&mut self.buf[..n], self.w, self.h)
        }
        /// Panics if any guard-band sentinel was overwritten.
        pub fn assert_guard_intact(&self) {
            let n = area(self.w, self.h);
            assert!(
                self.buf[n..].iter().all(|&v| v == SENTINEL),
                "guard band corrupted — an OOB write escaped the logical {}x{} region",
                self.w,
                self.h
            );
        }
    }

    #[test]
    fn new_validates_len_matches_w_times_h() {
        // This IS the "stale dimensions unrepresentable" property: a slice can only become a
        // RasterBuf paired with the w/h that actually describe its length.
        let mut good = vec![0u32; 6];
        let _ = PixelBuf::new(&mut good, 3, 2); // fine, must not panic
    }

    #[test]
    #[should_panic(expected = "slice len")]
    fn new_rejects_mismatched_dimensions() {
        // A caller cannot pair a 6-word slice with stale 4x4 dims — construction itself refuses.
        let mut stale = vec![0u32; 6];
        let _ = PixelBuf::new(&mut stale, 4, 4);
    }

    #[test]
    fn zero_size_buffer_all_ops_are_noops_not_panics() {
        let mut buf = Guarded::new(0, 0);
        let mut pb = buf.pixel_buf();
        assert_eq!(pb.get(0, 0), None);
        pb.put(0, 0, 0xFFFF_FFFF); // must not panic, must not write
        assert!(pb.row_mut(0).is_none());
        assert!(pb.row_range_mut(0, -5, 5).is_none());
        for &(x, y) in &[
            (0, 0),
            (-1, -1),
            (i32::MIN, i32::MIN),
            (i32::MAX, i32::MAX),
        ] {
            pb.put(x, y, 0x1234);
            assert_eq!(pb.get(x, y), None);
        }
        buf.assert_guard_intact();
    }

    #[test]
    fn one_by_one_buffer_bounds() {
        let mut buf = Guarded::new(1, 1);
        {
            let mut pb = buf.pixel_buf();
            pb.put(0, 0, 0xAABBCCDD);
            assert_eq!(pb.get(0, 0), Some(0xAABBCCDD));
            // every neighbour is out of bounds for a 1x1 buffer
            for &(x, y) in &[(1, 0), (-1, 0), (0, 1), (0, -1), (1, 1), (-1, -1)] {
                pb.put(x, y, 0xFFFF_FFFF);
                assert_eq!(pb.get(x, y), None);
            }
        }
        buf.assert_guard_intact();
    }

    #[test]
    fn extreme_coordinates_never_panic_or_corrupt() {
        let mut buf = Guarded::new(37, 29);
        let mut pb = buf.pixel_buf();
        let extremes = [
            i32::MIN,
            i32::MIN + 1,
            -1_000_000_000,
            -1,
            0,
            1,
            1_000_000_000,
            i32::MAX - 1,
            i32::MAX,
        ];
        for &x in &extremes {
            for &y in &extremes {
                // get/put must never panic regardless of how extreme the coordinates are —
                // this is exactly the stale-dimensions-turned-huge-offset scenario.
                pb.put(x, y, 0x1);
                let _ = pb.get(x, y);
            }
        }
        drop(pb);
        buf.assert_guard_intact();
    }

    #[test]
    fn row_mut_and_row_range_mut_clamp_correctly() {
        let mut buf = Guarded::new(10, 4);
        let mut pb = buf.pixel_buf();
        assert_eq!(pb.row_mut(-1), None);
        assert_eq!(pb.row_mut(4), None);
        assert_eq!(pb.row_mut(0).unwrap().len(), 10);

        // a run that starts before 0 and ends past w-1 clamps to the full row
        assert_eq!(pb.row_range_mut(1, -50, 500).unwrap().len(), 10);
        // a run fully to the left of the buffer is empty
        assert!(pb.row_range_mut(1, -50, -5).is_none());
        // a run fully to the right of the buffer is empty
        assert!(pb.row_range_mut(1, 50, 500).is_none());
        // an inverted range (x1 < x0) is empty, not a panic/underflow
        assert!(pb.row_range_mut(1, 8, 2).is_none());

        for v in pb.row_range_mut(2, 3, 6).unwrap() {
            *v = 0x42;
        }
        drop(pb);
        // exactly indices 3..=6 of row 2 got written
        let n = 10usize;
        for x in 0..10usize {
            let v = buf.buf[2 * n + x];
            if (3..=6).contains(&x) {
                assert_eq!(v, 0x42);
            } else {
                assert_eq!(v, 0);
            }
        }
        buf.assert_guard_intact();
    }

    #[test]
    fn get_unchecked_put_unchecked_roundtrip_when_in_bounds() {
        let mut buf = Guarded::new(5, 5);
        let mut pb = buf.pixel_buf();
        unsafe {
            pb.put_unchecked(2, 2, 0x99);
            assert_eq!(pb.get_unchecked(2, 2), 0x99);
        }
        drop(pb);
        buf.assert_guard_intact();
    }

    #[test]
    fn as_view_reads_the_same_memory_as_the_buf() {
        let mut buf = Guarded::new(4, 4);
        {
            let mut pb = buf.pixel_buf();
            pb.put(1, 1, 0x7777);
            let view = pb.as_view();
            assert_eq!(view.get(1, 1), Some(0x7777));
            assert_eq!(view.get(-1, -1), None);
        }
        buf.assert_guard_intact();
    }

    #[test]
    fn field_buf_works_for_f32_too() {
        let mut data = vec![f32::MAX; 9];
        let mut fb = FieldBuf::new(&mut data, 3, 3);
        fb.put(1, 1, 0.5);
        assert_eq!(fb.get(1, 1), Some(0.5));
        fb.put(100, 100, -1.0); // off-buffer, silently skipped
        assert_eq!(fb.get(100, 100), None);
    }

    #[test]
    fn radius_larger_than_buffer_stays_within_bounds() {
        // Simulate a raster primitive that clamps a huge radius against the buffer's own w/h
        // (as stamp_segment/ping_ring do) using RasterBuf's row_range_mut, and confirm nothing
        // outside the logical region is ever touched even when the requested radius dwarfs it.
        let mut buf = Guarded::new(6, 6);
        let mut pb = buf.pixel_buf();
        let h = pb.h();
        let (cx, cy) = (3, 3);
        let radius = 10_000_i32; // wildly larger than the 6x6 buffer
        let y0 = (cy - radius).max(0);
        let y1 = (cy + radius).min(h - 1);
        for y in y0..=y1 {
            let x0 = cx - radius;
            let x1 = cx + radius;
            if let Some(row) = pb.row_range_mut(y, x0, x1) {
                for v in row {
                    *v = 0xFF;
                }
            }
        }
        drop(pb);
        assert!(buf.buf[..36].iter().all(|&v| v == 0xFF));
        buf.assert_guard_intact();
    }
}
