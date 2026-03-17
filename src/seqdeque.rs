use std::{
    collections::{VecDeque, vec_deque::Iter},
    ops::{Bound, Index, IndexMut, RangeBounds},
};

#[derive(Clone, Default)]
pub struct SeqDeque<T>(VecDeque<T>, usize);

impl<T: std::fmt::Debug> std::fmt::Debug for SeqDeque<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

trait BoundExt {
    fn sub_offset(self, offset: usize) -> Bound<usize>;
}

impl BoundExt for Bound<&usize> {
    fn sub_offset(self, offset: usize) -> Bound<usize> {
        match self {
            Bound::Included(&x) => Bound::Included(x - offset),
            Bound::Excluded(&x) => Bound::Excluded(x - offset),
            Bound::Unbounded => Bound::Unbounded,
        }
    }
}

impl<T> SeqDeque<T> {
    pub fn new() -> SeqDeque<T> {
        SeqDeque(VecDeque::new(), 0)
    }

    pub fn push_back(&mut self, value: T) {
        self.0.push_back(value);
    }

    pub fn partition_point<P>(&self, pred: P) -> usize
    where
        P: FnMut(&T) -> bool,
    {
        self.0.partition_point(pred) + self.1
    }

    pub fn remove_front(&mut self, index: usize) {
        self.0.drain(0..(index - self.1));
        self.1 = index;
    }

    pub fn start_index(&self) -> usize {
        self.1
    }

    pub fn end_index(&self) -> usize {
        self.1 + self.0.len()
    }

    pub fn range<R>(&self, range: R) -> Iter<'_, T>
    where
        R: RangeBounds<usize>,
    {
        let start = range.start_bound().sub_offset(self.1);
        let end = range.end_bound().sub_offset(self.1);
        self.0.range((start, end))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn front(&self) -> Option<&T> {
        self.0.front()
    }

    pub fn back(&self) -> Option<&T> {
        self.0.back()
    }

    pub fn iter(&self) -> Iter<'_, T> {
        self.0.iter()
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        if index >= self.start_index() && index < self.end_index() {
            Some(&self.0[index - self.1])
        } else {
            None
        }
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        if index >= self.start_index() && index < self.end_index() {
            Some(&mut self.0[index - self.1])
        } else {
            None
        }
    }
}

impl<T> Index<usize> for SeqDeque<T> {
    type Output = T;

    #[inline]
    fn index(&self, index: usize) -> &T {
        &self.0[index - self.1]
    }
}

impl<T> IndexMut<usize> for SeqDeque<T> {
    #[inline]
    fn index_mut(&mut self, index: usize) -> &mut T {
        &mut self.0[index - self.1]
    }
}
