#![no_std]
#![allow(internal_features)]
#![feature(allocator_api, core_intrinsics)]
extern crate alloc;

const RC_SINGLE: usize = 4;
const WEAK: usize = 2;
const CLOSED: usize = 1;
const MAX_REFCOUNT: usize = (usize::MAX as usize) & !(RC_SINGLE - 1);

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
    unsafe fn release_weak<A: Allocator>(this: NonNull<Self>, alloc: A) {
        let old = this.as_ref().weak.load(Ordering::Relaxed);
        if old == 1 || this.as_ref().weak.fetch_sub(1, Ordering::AcqRel) == 0 {
            Box::from_raw_in(this.as_ptr(), alloc);
        }
    }
    unsafe fn release_strong<A: Allocator>(mut this: NonNull<Self>, alloc: A) {
        let old = this.as_ref().strong.fetch_sub(RC_SINGLE, Ordering::AcqRel);
        if old > RC_SINGLE + WEAK {
            return;
        }
        if old & WEAK == 0 {
            core::ptr::drop_in_place(&mut this.as_mut().data);
            Box::from_raw_in(this.as_ptr(), alloc);
            return;
        }
    }
}
