use std::{array, collections::VecDeque, sync::Arc};

const CHUNK_SIZE: usize = 1024;

#[derive(Clone)]
pub struct ChunkedDeque<T> {
    chunks: VecDeque<Arc<[Option<Arc<T>>; CHUNK_SIZE]>>,
    start_offset: isize,
    end_offset: isize,
}

impl<T> Default for ChunkedDeque<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> std::fmt::Debug for ChunkedDeque<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ChunkedDeque({}..{}, {} chunks)",
            self.start_index(),
            self.end_index(),
            self.chunks.len()
        )
    }
}

impl<T> ChunkedDeque<T> {
    pub fn new() -> Self {
        Self {
            chunks: VecDeque::new(),
            start_offset: 0,
            end_offset: 0,
        }
    }

    pub fn start_index(&self) -> isize {
        self.start_offset
    }

    pub fn end_index(&self) -> isize {
        self.end_offset
    }

    pub fn len(&self) -> usize {
        (self.end_offset - self.start_offset) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.start_offset == self.end_offset
    }

    fn index_to_chunk_and_offset(&self, index: isize) -> Option<(usize, usize)> {
        if index < self.start_index() || index >= self.end_index() {
            return None;
        }

        let absolute_chunk = index.div_euclid(CHUNK_SIZE as isize);
        let chunk_offset = self.start_offset.div_euclid(CHUNK_SIZE as isize);
        let chunk_index = (absolute_chunk - chunk_offset) as usize;
        let offset_in_chunk = index.rem_euclid(CHUNK_SIZE as isize) as usize;

        if chunk_index >= self.chunks.len() {
            return None;
        }

        Some((chunk_index, offset_in_chunk))
    }

    pub fn get(&self, index: isize) -> Option<&Arc<T>> {
        let (chunk_idx, offset) = self.index_to_chunk_and_offset(index)?;
        Some(self.chunks[chunk_idx][offset].as_ref().unwrap())
    }

    pub fn push_back(&mut self, arc_value: Arc<T>) {
        let end_idx = self.end_offset;
        let end_offset_in_chunk = end_idx.rem_euclid(CHUNK_SIZE as isize) as usize;

        if end_offset_in_chunk == 0 {
            let new_chunk = Arc::new(array::from_fn(|_| None));
            self.chunks.push_back(new_chunk);
            let chunk = Arc::make_mut(self.chunks.back_mut().unwrap());
            chunk[0] = Some(arc_value);
        } else {
            let chunk = Arc::make_mut(self.chunks.back_mut().unwrap());
            chunk[end_offset_in_chunk] = Some(arc_value);
        }

        self.end_offset += 1;
    }

    pub fn push_front(&mut self, arc_value: Arc<T>) {
        let new_start_idx = self.start_offset - 1;
        let new_start_offset_in_chunk = new_start_idx.rem_euclid(CHUNK_SIZE as isize) as usize;

        if new_start_offset_in_chunk == CHUNK_SIZE - 1 {
            let new_chunk = Arc::new(std::array::from_fn(|_| None));
            self.chunks.push_front(new_chunk);
            let chunk = Arc::make_mut(self.chunks.front_mut().unwrap());
            chunk[CHUNK_SIZE - 1] = Some(arc_value);
        } else {
            let chunk = Arc::make_mut(self.chunks.front_mut().unwrap());
            chunk[new_start_offset_in_chunk] = Some(arc_value);
        }

        self.start_offset -= 1;
    }

    pub fn pop_back(&mut self) -> Option<Arc<T>> {
        let back_idx = self.end_offset - 1;
        let (chunk_idx, offset) = self.index_to_chunk_and_offset(back_idx)?;

        let value = self.chunks[chunk_idx][offset].clone().unwrap();

        self.end_offset -= 1;

        if offset == 0 {
            self.chunks.pop_back();
        }

        Some(value)
    }

    pub fn pop_front(&mut self) -> Option<Arc<T>> {
        let start_idx = self.start_offset;
        let (chunk_idx, offset) = self.index_to_chunk_and_offset(start_idx)?;

        let value = self.chunks[chunk_idx][offset].clone().unwrap();

        self.start_offset += 1;

        if offset == CHUNK_SIZE - 1 {
            self.chunks.pop_front();
        }

        Some(value)
    }

    pub fn front(&self) -> Option<&Arc<T>> {
        self.get(self.start_offset)
    }

    pub fn back(&self) -> Option<&Arc<T>> {
        self.get(self.end_offset - 1)
    }

    pub fn structural_eq_range(&self, other: &Self, range: std::ops::Range<isize>) -> bool {
        let std::ops::Range { start, end } = range;

        assert!(start >= self.start_index() && start <= self.end_index());
        assert!(start >= other.start_index() && start <= other.end_index());
        assert!(end >= self.start_index() && end <= self.end_index());
        assert!(end >= other.start_index() && end <= other.end_index());
        assert!(start <= end);

        let mut cur = start;
        let (mut self_chunk_idx, mut offset) = self.index_to_chunk_and_offset(cur).unwrap();
        let (mut other_chunk_idx, offset2) = other.index_to_chunk_and_offset(cur).unwrap();
        debug_assert_eq!(offset, offset2);

        while cur < end {
            let self_chunk = &self.chunks[self_chunk_idx];
            let other_chunk = &other.chunks[other_chunk_idx];
            if Arc::ptr_eq(self_chunk, other_chunk) {
                // Entire chunk is equal, advance to the start of the next chunk
                cur = ((cur.div_euclid(CHUNK_SIZE as isize) + 1) * CHUNK_SIZE as isize).min(end);
                self_chunk_idx += 1;
                other_chunk_idx += 1;
                offset = 0;
            } else {
                // Check each element within the current chunk
                while cur < end && offset < CHUNK_SIZE {
                    let self_arc = self_chunk[offset].as_ref().unwrap();
                    let other_arc = other_chunk[offset].as_ref().unwrap();
                    if !Arc::ptr_eq(self_arc, other_arc) {
                        return false;
                    }
                    offset += 1;
                    cur += 1;
                }
                // It's okay for these to diverge from cur if we are at the end
                // since we will immediately leave the loop
                self_chunk_idx += 1;
                other_chunk_idx += 1;
                offset = 0;
            }
        }
        true
    }

    pub fn remove_front(&mut self, index: isize) {
        if index <= self.start_index() {
            return;
        }

        if index >= self.end_index() {
            self.chunks.clear();
            self.start_offset = index;
            self.end_offset = index;
            return;
        }

        let (chunk_idx, _offset) = self.index_to_chunk_and_offset(index).unwrap();
        self.chunks.drain(0..chunk_idx);
        self.start_offset = index;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_operations() {
        let mut deque = ChunkedDeque::new();

        deque.push_back(Arc::new(1));
        deque.push_back(Arc::new(2));
        deque.push_back(Arc::new(3));

        assert_eq!(deque.len(), 3);
        assert_eq!(**deque.front().unwrap(), 1);
        assert_eq!(**deque.back().unwrap(), 3);

        deque.push_front(Arc::new(0));
        assert_eq!(deque.len(), 4);
        assert_eq!(**deque.front().unwrap(), 0);

        let val = deque.pop_back().unwrap();
        assert_eq!(*val, 3);
        assert_eq!(deque.len(), 3);

        let val = deque.pop_front().unwrap();
        assert_eq!(*val, 0);
        assert_eq!(deque.len(), 2);
    }

    #[test]
    fn test_structural_equality() {
        let mut deque1 = ChunkedDeque::new();

        for i in 0..10 {
            deque1.push_front(Arc::new(i));
        }

        for i in 0..10 {
            deque1.push_back(Arc::new(i));
        }

        let deque2 = deque1.clone();

        assert!(deque1.structural_eq_range(&deque2, -10isize..10isize));

        let mut deque3 = deque1.clone();
        deque3.push_back(Arc::new(10));
        // Replace front element with a new one (same value)
        deque3.pop_front();
        deque3.push_front(Arc::new(9));
        // deque1 has -10..10, deque3 has -10..11
        // They should be structurally equal in the range -9..10
        assert!(deque1.structural_eq_range(&deque3, -9isize..10isize));

        // They should be structurally unequal in the range -10..10
        // since the front element was replaced
        assert!(!deque1.structural_eq_range(&deque3, -10isize..10isize));
    }

    #[test]
    fn test_clone_o1() {
        let mut deque = ChunkedDeque::new();
        for i in 0..1000 {
            deque.push_back(Arc::new(i));
        }

        let cloned = deque.clone();

        assert!(deque.structural_eq_range(&cloned, 0isize..1000isize));
    }

    #[test]
    fn test_remove_front() {
        let mut deque = ChunkedDeque::new();

        for i in 0..20 {
            deque.push_back(Arc::new(i));
        }

        assert_eq!(deque.start_index(), 0);
        assert_eq!(deque.end_index(), 20);

        deque.remove_front(5);

        assert_eq!(deque.start_index(), 5);
        assert_eq!(deque.end_index(), 20);
        assert_eq!(deque.len(), 15);
        assert_eq!(**deque.front().unwrap(), 5);
    }

    #[test]
    fn test_push_front_multiple() {
        let mut deque = ChunkedDeque::new();

        for i in 0..10 {
            deque.push_front(Arc::new(i));
        }

        assert_eq!(deque.len(), 10);
        assert_eq!(**deque.front().unwrap(), 9);
        assert_eq!(**deque.back().unwrap(), 0);
    }

    #[test]
    fn test_crossing_chunk_boundaries() {
        let mut deque = ChunkedDeque::new();

        for i in 0..CHUNK_SIZE * 2 + 10 {
            deque.push_back(Arc::new(i));
        }

        for i in 0..CHUNK_SIZE * 2 + 10 {
            assert_eq!(**deque.get(i as isize).unwrap(), i);
        }

        assert_eq!(deque.chunks.len(), 3);
    }

    #[test]
    fn test_negative_indices() {
        let mut deque = ChunkedDeque::new();

        for i in 0..10 {
            deque.push_back(Arc::new(i));
        }

        for i in 0..5 {
            deque.push_front(Arc::new(100 + i));
        }

        assert_eq!(deque.start_index(), -5);
        assert_eq!(deque.end_index(), 10);
        assert_eq!(deque.len(), 15);
        assert_eq!(**deque.get(-5).unwrap(), 104);
        assert_eq!(**deque.get(-1).unwrap(), 100);
        assert_eq!(**deque.get(0).unwrap(), 0);
        assert_eq!(**deque.get(9).unwrap(), 9);
    }
}
