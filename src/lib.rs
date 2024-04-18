#![no_std]
#![allow(internal_features)]
#![feature(allocator_api, core_intrinsics)]
extern crate alloc;

const RC_SINGLE: usize = 4;
const WEAK: usize = 2;
const CLOSED: usize = 1;
const MAX_STRONG_COUNT: usize = isize::MAX as usize >> 2; // 61 bits for strong count
const MAX_WEAK_COUNT: usize = isize::MAX as usize; // 63 bits for weak count

#[cfg(test)]
mod tests;

use core::{
    alloc::Allocator,
    marker::PhantomData,
    mem::{ManuallyDrop, MaybeUninit},
    ops::Deref,
    panic::{RefUnwindSafe, UnwindSafe},
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use alloc::{alloc::Global, boxed::Box};

struct ArcInner<T: ?Sized> {
    strong: AtomicUsize,
    weak: AtomicUsize,
    data: ManuallyDrop<T>,
}

unsafe impl<T: ?Sized + Sync + Send> Send for ArcInner<T> {}
unsafe impl<T: ?Sized + Sync + Send> Sync for ArcInner<T> {}

struct AllocHolder<A: Allocator>(MaybeUninit<A>);

impl<A: Allocator> AllocHolder<A> {
    fn new(alloc: A) -> Self {
        Self(MaybeUninit::new(alloc))
    }
    #[inline]
    unsafe fn clone(this: &Self) -> Self
    where
        A: Clone,
    {
        Self::new(this.0.assume_init_ref().clone())
    }
    #[inline]
    unsafe fn take(this: &mut Self) -> A {
        this.0.assume_init_read()
    }
}

pub struct Arc<T: ?Sized, A: Allocator = Global> {
    ptr: NonNull<ArcInner<T>>,
    phantom: PhantomData<ArcInner<T>>,
    alloc: AllocHolder<A>,
}

pub struct Weak<T: ?Sized, A: Allocator = Global> {
    ptr: NonNull<ArcInner<T>>,
    alloc: AllocHolder<A>,
}

impl<T> Arc<T> {
    pub fn new(data: T) -> Self {
        Self::new_in(data, Global)
    }
}

impl<T, A: Allocator> Arc<T, A> {
    pub fn new_in(data: T, alloc: A) -> Self {
        let boxed = Box::new_in(
            ArcInner {
                strong: AtomicUsize::new(RC_SINGLE),
                weak: AtomicUsize::new(0),
                data: ManuallyDrop::new(data),
            },
            alloc,
        );
        let (ptr, alloc) = Box::into_raw_with_allocator(boxed);
        let ptr = unsafe { NonNull::new_unchecked(ptr) };
        let alloc = AllocHolder::new(alloc);
        Self {
            ptr,
            phantom: PhantomData,
            alloc,
        }
    }
}

impl<T: ?Sized, A: Allocator> Arc<T, A> {
    pub fn downgrade(this: &Self) -> Weak<T, A>
    where
        A: Clone,
    {
        unsafe {
            ArcInner::acquire_weak(this.ptr);
            Weak {
                ptr: this.ptr,
                alloc: AllocHolder::clone(&this.alloc),
            }
        }
    }
    fn inner(&self) -> &ArcInner<T> {
        unsafe { self.ptr.as_ref() }
    }
    pub fn strong_count(&self) -> usize {
        self.inner().strong.load(Ordering::Acquire) >> 2
    }
    pub fn weak_count(&self) -> usize {
        self.inner().weak.load(Ordering::Acquire)
    }
}

impl<T: ?Sized, A: Allocator> Weak<T, A> {
    pub fn upgrade(&self) -> Option<Arc<T, A>>
    where
        A: Clone,
    {
        unsafe {
            if ArcInner::acquire_strong_from_weak(self.ptr) {
                Some(Arc {
                    ptr: self.ptr,
                    phantom: PhantomData,
                    alloc: AllocHolder::clone(&self.alloc),
                })
            } else {
                None
            }
        }
    }
}

impl<T: ?Sized, A: Allocator> Drop for Arc<T, A> {
    fn drop(&mut self) {
        unsafe { ArcInner::release_strong(self.ptr, AllocHolder::take(&mut self.alloc)) };
    }
}

impl<T: ?Sized, A: Allocator> Drop for Weak<T, A> {
    fn drop(&mut self) {
        unsafe { ArcInner::release_weak(self.ptr, AllocHolder::take(&mut self.alloc)) };
    }
}

impl<T: ?Sized, A: Allocator + Clone> Clone for Weak<T, A> {
    fn clone(&self) -> Self {
        unsafe {
            ArcInner::acquire_weak(self.ptr);
            Weak {
                ptr: self.ptr,
                alloc: AllocHolder::clone(&self.alloc),
            }
        }
    }
}

impl<T: ?Sized, A: Allocator + Clone> Clone for Arc<T, A> {
    fn clone(&self) -> Self {
        unsafe {
            ArcInner::acquire_strong(self.ptr);
            Arc {
                ptr: self.ptr,
                phantom: PhantomData,
                alloc: AllocHolder::clone(&self.alloc),
            }
        }
    }
}

unsafe impl<T: ?Sized + Sync + Send, A: Allocator + Send> Send for Arc<T, A> {}
unsafe impl<T: ?Sized + Sync + Send, A: Allocator + Sync> Sync for Arc<T, A> {}

unsafe impl<T: ?Sized + Sync + Send, A: Allocator + Send> Send for Weak<T, A> {}
unsafe impl<T: ?Sized + Sync + Send, A: Allocator + Sync> Sync for Weak<T, A> {}

impl<T: RefUnwindSafe + ?Sized, A: Allocator + UnwindSafe> UnwindSafe for Arc<T, A> {}

impl<T: ?Sized, A: Allocator> Deref for Arc<T, A> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        &self.inner().data
    }
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
        let old = this.as_ref().strong.fetch_sub(RC_SINGLE, Ordering::AcqRel);
        if old > RC_SINGLE + WEAK {
            // object is still alive, return
            return;
        }
        if old & WEAK == 0 {
            // this RC has never been weakly referenced
            // drop it immediately
            ManuallyDrop::drop(&mut this.as_mut().data);
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
            ManuallyDrop::drop(&mut this.as_mut().data);
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
