use std::cell::UnsafeCell;
use std::cmp::min;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::{iter, ptr};

/// Capacity of the first segment in the buffer.
const STARTING_SIZE: usize = 64;
/// Max capacity of a segment in the buffer.
const MAX_SIZE: usize = 262_144;

/// A segment in the buffer.
struct Segment<T> {
    /// Pointer to the next segment in the list, or `ptr::null`
    /// if it does not exist.
    next: AtomicPtr<Segment<T>>,
    /// Capacity of this segment.
    capacity: usize,
    /// Index of the next value to write into this
    /// segment. Note that this value may exceed
    /// `capacity`: if so, the buffer has been fully written.
    front: AtomicUsize,
    /// Index of the next value to read from this
    /// segment.
    ///
    /// If the value is greater than or equal to `capacity`,
    /// then the entire segment has been read.
    /// If the value is greater than or equal to `front`,
    /// then there are no values in this segment remaining
    /// to read.
    back: AtomicUsize,
    /// Array of values in this segment.
    ///
    /// This array has length `self.capacity`.
    array: Vec<UnsafeCell<MaybeUninit<T>>>,
}

impl<T> Drop for Segment<T> {
    fn drop(&mut self) {
        let front = min(self.capacity, *self.front.get_mut());
        let back = min(self.capacity, *self.back.get_mut());

        for i in back..front {
            unsafe {
                drop(ptr::read((&mut *self.array[i].get()).as_mut_ptr()));
            }
        }
    }
}

pub struct RawBuffer<T> {
    /// Pointer to the head segment.
    ///
    /// Do note that segments may exist past this
    /// head as linked by the `next` pointer.
    /// The head is only the segment to which values
    /// are currently written.
    ///
    /// This value must never be null.
    head: AtomicPtr<Segment<T>>,
    /// Pointer to the tail segment.
    ///
    /// This value must never be null.
    tail: AtomicPtr<Segment<T>>,
}

impl<T> RawBuffer<T> {
    pub fn new() -> Self {
        let head = new_segment(STARTING_SIZE);
        Self {
            head: AtomicPtr::new(head),
            tail: AtomicPtr::new(head),
        }
    }

    /// Pushes a value onto the buffer.
    ///
    /// # Safety
    /// Only other calls to `push` may execute concurrently.
    pub unsafe fn push(&self, value: T) {
        // Obtain a position in a segment.
        let (segment, index) = loop {
            let head = &mut *self.head.load(Ordering::Acquire);

            let position = head.front.fetch_add(1, Ordering::AcqRel);

            if position < head.capacity {
                break (head, position);
            } else {
                // Position is past the end of the segment. We do the following:
                // * If `head->next` is set, then there is another segment available.
                // Attempt to set it as the new head and continue the loop.
                // * Otherwise, there are no more available segments.
                // We allocate a new one and traverse the list forward
                // until there is a segment whose `next` pointer we can
                // set to the new segment (i.e. the old value isn't null).
                let next = head.next.load(Ordering::Acquire);
                if !next.is_null() {
                    self.head
                        .compare_and_swap(head as *mut _, next, Ordering::AcqRel);
                } else {
                    // Allocate new segment.
                    let new_segment = new_segment(min(MAX_SIZE, head.capacity * 2));

                    self.append_segment(new_segment);
                }
            }
        };

        // Write value into segment.
        let ptr = (&mut *segment.array[index].get()).as_mut_ptr();

        ptr::write(ptr, value);
    }

    /// Removes a value from the start of the buffer.
    ///
    /// # Safety
    /// Neither push operations or other pop operations may not run in parallel with this function.
    pub unsafe fn pop(&mut self) -> Option<T> {
        // No need for atomic operations, since we have unique access.
        let (segment, index) = loop {
            let segment = &mut **self.tail.get_mut();

            let index = *segment.back.get_mut();
            *segment.back.get_mut() += 1;

            if index >= *segment.front.get_mut() {
                *segment.back.get_mut() = *segment.front.get_mut();
                return None;
            }

            if index >= segment.capacity {
                *segment.back.get_mut() = 0;
                *segment.front.get_mut() = 0;
                if *self.head.get_mut() == segment as *mut _ {
                    return None;
                } else {
                    *self.tail.get_mut() = *segment.next.get_mut();
                    *segment.next.get_mut() = ptr::null_mut();
                    self.append_segment(segment);
                }
            } else {
                break (segment, index);
            }
        };

        let ptr = (&*segment.array[index].get()).as_ptr();
        Some(ptr::read(ptr))
    }

    /// Returns a raw iterator over segments.
    ///
    /// # Safety
    /// Neither push operations or other pop operations may not run in parallel with this function.
    pub fn iter(&mut self) -> RawIter<T> {
        let tail = *self.tail.get_mut();
        RawIter {
            buffer: self,
            segment: tail,
        }
    }

    /// Returns a raw parallel iterator over segments.
    ///
    /// # Safety
    /// Neither push operations or other pop operations may not run in parallel with this function.
    #[cfg(feature = "rayon")]
    pub fn par_iter(&mut self) -> ParRawIter<T> {
        let tail = *self.tail.get_mut();
        ParRawIter {
            buffer: self,
            segment: tail,
        }
    }

    unsafe fn append_segment(&self, segment: *mut Segment<T>) {
        // Traverse to the end of the list and add the new segment.
        let mut head = self.head.load(Ordering::Acquire);
        let mut next = ptr::null_mut::<Segment<T>>();
        while !head.is_null() && {
            next = (&*head)
                .next
                .compare_and_swap(ptr::null_mut(), segment, Ordering::AcqRel);
            !next.is_null()
        } {
            head = next;
        }
    }
}

impl<T> Drop for RawBuffer<T> {
    fn drop(&mut self) {
        let mut tail = *self.tail.get_mut();

        while !tail.is_null() {
            unsafe {
                let temp = *(&mut *tail).next.get_mut();
                drop(Box::from_raw(tail));
                tail = temp;
            }
        }
    }
}

fn new_segment<T>(capacity: usize) -> *mut Segment<T> {
    let boxed = Box::new(Segment {
        next: AtomicPtr::new(ptr::null_mut()),
        capacity,
        front: AtomicUsize::new(0),
        back: AtomicUsize::new(0),
        array: iter::repeat_with(|| UnsafeCell::new(MaybeUninit::uninit()))
            .take(capacity)
            .collect(),
    });

    Box::into_raw(boxed)
}

pub struct RawIter<'a, T> {
    #[allow(dead_code)]
    buffer: &'a RawBuffer<T>,
    segment: *mut Segment<T>,
}

impl<'a, T> Iterator for RawIter<'a, T> {
    type Item = &'a mut [T];

    fn next(&mut self) -> Option<Self::Item> {
        let segment = unsafe { self.segment.as_mut()? };

        let start = min(*segment.back.get_mut(), segment.capacity);
        let end = min(*segment.front.get_mut(), segment.capacity);

        let uninit_slice = &mut segment.array[start..end];

        // Sound because both UnsafeCell and MaybeUninit are repr(transparent).
        let slice = unsafe {
            std::mem::transmute::<&mut [UnsafeCell<MaybeUninit<T>>], &mut [T]>(uninit_slice)
        };

        self.segment = *segment.next.get_mut();

        Some(slice)
    }
}

#[cfg(feature = "rayon")]
pub use self::rayon::*;
#[cfg(feature = "rayon")]
mod rayon {
    use crate::seg_buffer::raw::{RawBuffer, RawIter, Segment};
    use rayon::iter::plumbing::{Consumer, Folder, UnindexedConsumer, UnindexedProducer};
    use rayon::iter::{plumbing, ParallelIterator};

    pub struct ParRawIter<'a, T> {
        pub(super) buffer: &'a RawBuffer<T>,
        pub(super) segment: *mut Segment<T>,
    }

    unsafe impl<'a, T> Send for ParRawIter<'a, T> where T: Send {}

    impl<'a, T> ParallelIterator for ParRawIter<'a, T>
    where
        T: Send,
    {
        type Item = &'a mut [T];

        fn drive_unindexed<C>(self, consumer: C) -> <C as Consumer<Self::Item>>::Result
        where
            C: UnindexedConsumer<Self::Item>,
        {
            plumbing::bridge_unindexed(self, consumer)
        }
    }

    impl<'a, T> ParRawIter<'a, T> {
        pub fn slice(&self) -> &'a mut [T] {
            RawIter {
                segment: self.segment,
                buffer: self.buffer,
            }
            .next()
            .unwrap()
        }
    }

    impl<'a, T> UnindexedProducer for ParRawIter<'a, T>
    where
        T: Send,
    {
        type Item = &'a mut [T];

        fn split(self) -> (Self, Option<Self>) {
            let next = unsafe {
                let ptr = *(&mut *self.segment).next.get_mut();
                ptr.as_mut()
            };

            let buffer = self.buffer;
            match next {
                Some(next) => (
                    self,
                    Some(Self {
                        buffer,
                        segment: next as *mut _,
                    }),
                ),
                None => (self, None),
            }
        }

        fn fold_with<F>(self, folder: F) -> F
        where
            F: Folder<Self::Item>,
        {
            let slice = self.slice();

            folder.consume(slice)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        let mut buffer = RawBuffer::new();

        for i in 0..1024 {
            unsafe {
                buffer.push(i);
                assert_eq!(buffer.pop(), Some(i));
            }
        }

        for i in 0..65536 {
            unsafe { buffer.push(i) };
        }

        for i in 0..65536 {
            assert_eq!(unsafe { buffer.pop() }, Some(i));
        }
    }
}
