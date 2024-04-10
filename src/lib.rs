#![no_std]
#![allow(internal_features)]
#![feature(allocator_api, core_intrinsics)]
extern crate alloc;

const RC_SINGLE: usize = 4;
const WEAK: usize = 2;
const CLOSED: usize = 1;
const MAX_STRONG_COUNT: usize = isize::MAX as usize >> 2; // 61 bits for strong count
const MAX_WEAK_COUNT: usize = isize::MAX as usize; // 63 bits for weak count

use core::{
    alloc::{Allocator, Layout},
    marker::PhantomData,
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use alloc::{alloc::Global, boxed::Box};

struct ArcInner<T: ?Sized> {
    strong: AtomicUsize,
    weak: AtomicUsize,
    data: T,
}

unsafe impl<T: ?Sized + Sync + Send> Send for ArcInner<T> {}
unsafe impl<T: ?Sized + Sync + Send> Sync for ArcInner<T> {}

pub struct Arc<T: ?Sized, A: Allocator = Global> {
    ptr: NonNull<ArcInner<T>>,
    phantom: PhantomData<ArcInner<T>>,
    alloc: A,
}

impl<T: ?Sized> ArcInner<T> {
    #[inline]
    unsafe fn release_weak<A: Allocator>(this: NonNull<Self>, alloc: A) {
        let old = this.as_ref().weak.load(Ordering::Acquire);
        if old == 1 || this.as_ref().weak.fetch_sub(1, Ordering::AcqRel) == 0 {
            Box::from_raw_in(this.as_ptr(), alloc);
        }
    }
    #[inline]
    unsafe fn release_strong<A: Allocator>(mut this: NonNull<Self>, alloc: A) {
        let old = this.as_ref().strong.fetch_sub(RC_SINGLE, Ordering::Acquire);
        if old > RC_SINGLE + WEAK {
            // object is still alive, return
            return;
        }
        if old & WEAK == 0 {
            // this RC has never been weakly referenced
            // drop it immediately
            core::ptr::drop_in_place(&mut this.as_mut().data);
            Box::from_raw_in(this.as_ptr(), alloc);
            return;
        }
        if this
            .as_ref()
            .strong
            .compare_exchange(WEAK, CLOSED, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            // Previous CAS successfully closed the object. Thus, it is our responsibility to drop the associated data.
            core::ptr::drop_in_place(&mut this.as_mut().data);
        }
        // Release the implicitly held weak reference
        Self::release_weak(this, alloc);
    }
    #[inline]
    unsafe fn acquire_weak(this: NonNull<Self>) {
        // it is ok to use relaxed ordering since we are to do compare_exchange later on
        let old = this.as_ref().weak.load(Ordering::Relaxed);
        if old == 0 {
            if this
                .as_ref()
                .weak
                .compare_exchange(old, 2, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                // acquire the implicit weak reference
                this.as_ref().strong.fetch_add(WEAK, Ordering::Release);
                return;
            }
        }
        // acquire the explicit weak reference
        let weak = this.as_ref().weak.fetch_add(1, Ordering::Relaxed);
        if core::intrinsics::unlikely(weak > MAX_WEAK_COUNT) {
            core::intrinsics::abort();
        }
    }
    #[inline]
    unsafe fn acquire_strong(this: NonNull<Self>) {
        let val = this.as_ref().strong.fetch_add(RC_SINGLE, Ordering::Relaxed);
        if core::intrinsics::unlikely((val >> 2) > MAX_STRONG_COUNT) {
            core::intrinsics::abort();
        }
    }
    #[inline]
    unsafe fn acquire_strong_from_weak(this: NonNull<Self>) -> bool {
        let old = this.as_ref().strong.fetch_add(RC_SINGLE, Ordering::Relaxed);
        if old & CLOSED != 0 {
            return false;
        }
        if old < RC_SINGLE {
            // only one implicit weak reference
            Self::acquire_weak(this);
        }
        true
    }
}
