use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{array, collections::VecDeque, ops::Index, sync::Arc};

const CHUNK_SIZE: usize = 256;

// TODO: Replace Option<T> with MaybeUninit<T>
#[derive(Clone)]
pub struct ChunkedDeque<T> {
    chunks: VecDeque<Arc<[Option<T>; CHUNK_SIZE]>>,
    start_index: isize,
    end_index: isize,
}

pub struct ChunkedDequeIter<'a, T> {
    deque: &'a ChunkedDeque<T>,
    current_chunk_idx: usize,
    current_offset: usize,
    end_chunk_idx: usize,
    end_offset: usize,
}

impl<'a, T> Iterator for ChunkedDequeIter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        // Check if we've reached the end
        if self.current_chunk_idx > self.end_chunk_idx
            || (self.current_chunk_idx == self.end_chunk_idx
                && self.current_offset >= self.end_offset)
        {
            return None;
        }

        // Get the current element
        let chunk = self.deque.chunks.get(self.current_chunk_idx)?;
        let element = chunk[self.current_offset].as_ref()?;

        // Advance the iterator
        self.current_offset += 1;
        if self.current_offset >= CHUNK_SIZE {
            self.current_offset = 0;
            self.current_chunk_idx += 1;
        }

        Some(element)
    }
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
            start_index: 0,
            end_index: 0,
        }
    }

    pub fn starting_at(index: isize) -> Self {
        Self {
            chunks: VecDeque::new(),
            start_index: index,
            end_index: index,
        }
    }

    pub fn start_index(&self) -> isize {
        self.start_index
    }

    pub fn end_index(&self) -> isize {
        self.end_index
    }

    pub fn len(&self) -> usize {
        (self.end_index - self.start_index) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.start_index == self.end_index
    }

    fn index_to_chunk_and_offset(&self, index: isize) -> (usize, usize) {
        let absolute_chunk = index.div_euclid(CHUNK_SIZE as isize);
        let chunk_offset = self.start_index.div_euclid(CHUNK_SIZE as isize);
        let chunk_index = (absolute_chunk - chunk_offset) as usize;
        let offset_in_chunk = index.rem_euclid(CHUNK_SIZE as isize) as usize;

        (chunk_index, offset_in_chunk)
    }

    pub fn get(&self, index: isize) -> Option<&T> {
        if index < self.start_index || index >= self.end_index {
            return None;
        }
        let (chunk_idx, offset) = self.index_to_chunk_and_offset(index);
        Some(self.chunks[chunk_idx][offset].as_ref().unwrap())
    }

    pub fn push_back(&mut self, value: T)
    where
        T: Clone,
    {
        let end_idx = self.end_index;
        let end_offset_in_chunk = end_idx.rem_euclid(CHUNK_SIZE as isize) as usize;

        if self.is_empty() || end_offset_in_chunk == 0 {
            let new_chunk = Arc::new(array::from_fn(|_| None));
            self.chunks.push_back(new_chunk);
        }
        let chunk = Arc::make_mut(self.chunks.back_mut().unwrap());
        chunk[end_offset_in_chunk] = Some(value);

        self.end_index += 1;
    }

    pub fn push_front(&mut self, value: T)
    where
        T: Clone,
    {
        let new_start_idx = self.start_index - 1;
        let new_start_offset_in_chunk = new_start_idx.rem_euclid(CHUNK_SIZE as isize) as usize;

        if self.is_empty() || new_start_offset_in_chunk == CHUNK_SIZE - 1 {
            let new_chunk = Arc::new(std::array::from_fn(|_| None));
            self.chunks.push_front(new_chunk);
        }
        let chunk = Arc::make_mut(self.chunks.front_mut().unwrap());
        chunk[new_start_offset_in_chunk] = Some(value);

        self.start_index -= 1;
    }

    pub fn pop_back(&mut self) -> Option<T>
    where
        T: Clone,
    {
        if self.is_empty() {
            return None;
        }

        let back_idx = self.end_index - 1;
        let (chunk_idx, offset) = self.index_to_chunk_and_offset(back_idx);

        let chunk = Arc::make_mut(&mut self.chunks[chunk_idx]);
        let value = chunk[offset].take().unwrap();

        self.end_index -= 1;

        if offset == 0 {
            self.chunks.pop_back();
        }

        Some(value)
    }

    pub fn pop_front(&mut self) -> Option<T>
    where
        T: Clone,
    {
        if self.is_empty() {
            return None;
        }

        let start_idx = self.start_index;
        let (chunk_idx, offset) = self.index_to_chunk_and_offset(start_idx);

        let chunk = Arc::make_mut(&mut self.chunks[chunk_idx]);
        let value = chunk[offset].take().unwrap();

        self.start_index += 1;

        if offset == CHUNK_SIZE - 1 {
            self.chunks.pop_front();
        }

        Some(value)
    }

    pub fn front(&self) -> Option<&T> {
        self.get(self.start_index)
    }

    pub fn back(&self) -> Option<&T> {
        self.get(self.end_index - 1)
    }

    pub fn iter(&self) -> ChunkedDequeIter<'_, T> {
        let (start_chunk_idx, start_offset) = self.index_to_chunk_and_offset(self.start_index);
        let (end_chunk_idx, end_offset) = self.index_to_chunk_and_offset(self.end_index);

        ChunkedDequeIter {
            deque: self,
            current_chunk_idx: start_chunk_idx,
            current_offset: start_offset,
            end_chunk_idx,
            end_offset,
        }
    }

    pub fn range(&self, range: std::ops::Range<isize>) -> ChunkedDequeIter<'_, T> {
        let std::ops::Range { start, end } = range;
        assert!(start >= self.start_index && start <= self.end_index);
        assert!(end >= self.start_index && end <= self.end_index);
        assert!(start <= end);

        let (start_chunk_idx, start_offset) = self.index_to_chunk_and_offset(start);
        let (end_chunk_idx, end_offset) = self.index_to_chunk_and_offset(end);

        ChunkedDequeIter {
            deque: self,
            current_chunk_idx: start_chunk_idx,
            current_offset: start_offset,
            end_chunk_idx,
            end_offset,
        }
    }

    // Returns true if all of the following conditions are met:
    // * `self` overlaps `other`,
    // * `other` doesn't contain any data newer than `self`
    // * `self` doesn't contain any data older than `other`
    // * the data in the overlap matches
    //
    // Example of a "true" case:
    // self:     [defghijklmno]
    // other: [abcdefghijk]
    pub fn is_continuation_of(&self, other: &Self) -> bool
    where
        T: PartialEq,
    {
        if self.start_index < other.start_index || self.end_index < other.end_index {
            return false;
        }

        // Region of possible overlap
        let start = self.start_index;
        let end = other.end_index;

        if start >= end {
            // The two queues are disjoint
            return false;
        }

        let (mut self_chunk_idx, mut offset) = self.index_to_chunk_and_offset(start);
        let (mut other_chunk_idx, offset2) = other.index_to_chunk_and_offset(start);
        debug_assert_eq!(offset, offset2);
        let mut cur = start;

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
                    let self_val = self_chunk[offset].as_ref().unwrap();
                    let other_val = other_chunk[offset].as_ref().unwrap();
                    if self_val != other_val {
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
            self.start_index = index;
            self.end_index = index;
            return;
        }

        let (chunk_idx, _offset) = self.index_to_chunk_and_offset(index);
        self.chunks.drain(0..chunk_idx);
        self.start_index = index;
    }
}

impl<T> Index<isize> for ChunkedDeque<T> {
    type Output = T;

    fn index(&self, index: isize) -> &T {
        self.get(index).expect("index out of bounds")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_operations() {
        let mut deque = ChunkedDeque::new();

        deque.push_back(1);
        deque.push_back(2);
        deque.push_back(3);

        assert_eq!(deque.len(), 3);
        assert_eq!(*deque.front().unwrap(), 1);
        assert_eq!(*deque.back().unwrap(), 3);

        deque.push_front(0);
        assert_eq!(deque.len(), 4);
        assert_eq!(*deque.front().unwrap(), 0);

        let val = deque.pop_back().unwrap();
        assert_eq!(val, 3);
        assert_eq!(deque.len(), 3);

        let val = deque.pop_front().unwrap();
        assert_eq!(val, 0);
        assert_eq!(deque.len(), 2);
    }

    #[test]
    fn test_structural_equality() {
        let mut deque1 = ChunkedDeque::new();

        for i in 0..10 {
            deque1.push_front(i);
        }

        for i in 0..10 {
            deque1.push_back(i);
        }

        let mut deque2 = deque1.clone();
        deque2.push_back(10);
        // deque1 has -10..10, deque3 has -10..11
        // They should be equal in the overlapping range -10..10
        assert!(deque2.is_continuation_of(&deque1));

        // Replace front element with a different one
        deque2.pop_front();
        deque2.push_front(999);
        // Since the front element changed, deque3 is no longer a continuation of deque1
        assert!(!deque2.is_continuation_of(&deque1));
    }

    #[test]
    fn test_remove_front() {
        let mut deque = ChunkedDeque::new();

        for i in 0..20 {
            deque.push_back(i);
        }

        assert_eq!(deque.start_index(), 0);
        assert_eq!(deque.end_index(), 20);

        deque.remove_front(5);

        assert_eq!(deque.start_index(), 5);
        assert_eq!(deque.end_index(), 20);
        assert_eq!(deque.len(), 15);
        assert_eq!(*deque.front().unwrap(), 5);
    }

    #[test]
    fn test_push_front_multiple() {
        let mut deque = ChunkedDeque::new();

        for i in 0..10 {
            deque.push_front(i);
        }

        assert_eq!(deque.len(), 10);
        assert_eq!(*deque.front().unwrap(), 9);
        assert_eq!(*deque.back().unwrap(), 0);
    }

    #[test]
    fn test_crossing_chunk_boundaries() {
        let mut deque = ChunkedDeque::new();

        for i in 0..CHUNK_SIZE * 2 + 10 {
            deque.push_back(i);
        }

        for i in 0..CHUNK_SIZE * 2 + 10 {
            assert_eq!(*deque.get(i as isize).unwrap(), i);
        }

        assert_eq!(deque.chunks.len(), 3);
    }

    #[test]
    fn test_negative_indices() {
        let mut deque = ChunkedDeque::new();

        for i in 0..10 {
            deque.push_back(i);
        }

        for i in 0..5 {
            deque.push_front(100 + i);
        }

        assert_eq!(deque.start_index(), -5);
        assert_eq!(deque.end_index(), 10);
        assert_eq!(deque.len(), 15);
        assert_eq!(*deque.get(-5).unwrap(), 104);
        assert_eq!(*deque.get(-1).unwrap(), 100);
        assert_eq!(*deque.get(0).unwrap(), 0);
        assert_eq!(*deque.get(9).unwrap(), 9);
    }
}

impl<T: Serialize> Serialize for ChunkedDeque<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("ChunkedDeque", 2)?;

        // Collect all elements into a Vec
        let elements: Vec<&T> = self.iter().collect();
        state.serialize_field("elements", &elements)?;
        state.serialize_field("start_index", &self.start_index)?;
        state.end()
    }
}

impl<'de, T: Deserialize<'de> + Clone> Deserialize<'de> for ChunkedDeque<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::{self, MapAccess, Visitor};

        struct ChunkedDequeVisitor<T>(std::marker::PhantomData<T>);

        impl<'de, T: Deserialize<'de> + Clone> Visitor<'de> for ChunkedDequeVisitor<T> {
            type Value = ChunkedDeque<T>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("struct ChunkedDeque")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut elements: Option<Vec<T>> = None;
                let mut start_index: Option<isize> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "elements" => {
                            elements = Some(map.next_value()?);
                        }
                        "start_index" => {
                            start_index = Some(map.next_value()?);
                        }
                        _ => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }

                let elements = elements.ok_or_else(|| de::Error::missing_field("elements"))?;
                let start_index =
                    start_index.ok_or_else(|| de::Error::missing_field("start_index"))?;

                // Reconstruct the ChunkedDeque by pushing elements
                let mut deque = ChunkedDeque::new();
                deque.start_index = start_index;
                deque.end_index = start_index;

                for element in elements {
                    deque.push_back(element);
                }

                Ok(deque)
            }
        }

        const FIELDS: &[&str] = &["elements", "start_index"];
        deserializer.deserialize_struct(
            "ChunkedDeque",
            FIELDS,
            ChunkedDequeVisitor(std::marker::PhantomData),
        )
    }
}
