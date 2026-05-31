use std::{
    alloc::{self, Layout},
    ptr::{self, NonNull},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct Bitmap(u64);

impl Bitmap {
    pub const EMPTY: Self = Self(0);

    pub const fn len(self) -> usize {
        self.0.count_ones() as usize
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn has(self, bit: usize) -> bool {
        debug_assert!(bit < 64);
        (self.0 >> bit) & 1 == 1
    }

    /// Number of set bits strictly below `bit`, i.e. the index of `bit`'s entry in the entries
    /// array.
    pub fn rank(self, bit: usize) -> usize {
        debug_assert!(bit < 64);
        (self.0 & ((1u64 << bit) - 1)).count_ones() as usize
    }

    pub fn with_set(self, bit: usize) -> Self {
        debug_assert!(bit < 64);
        debug_assert!(!self.has(bit));
        Self(self.0 | (1u64 << bit))
    }

    pub fn with_clear(self, bit: usize) -> Self {
        debug_assert!(bit < 64);
        debug_assert!(self.has(bit));
        Self(self.0 & !(1u64 << bit))
    }
}

#[repr(C)]
struct SparseArrayInner<T> {
    bitmap: Bitmap,
    entries: [T; 0],
}

pub struct SparseArray<T>(NonNull<SparseArrayInner<T>>);

// SAFETY: SparseArray owns its allocation exclusively.
unsafe impl<T: Send> Send for SparseArray<T> {}
unsafe impl<T: Sync> Sync for SparseArray<T> {}

impl<T> SparseArray<T> {
    pub fn new<I: IntoIterator<Item = T>>(bitmap: Bitmap, entries: I) -> Self {
        let len = bitmap.len();
        let layout = Self::layout(len);
        let nullable = unsafe { alloc::alloc(layout) };
        let non_null = match NonNull::new(nullable) {
            Some(ptr) => ptr.cast::<SparseArrayInner<T>>(),
            None => alloc::handle_alloc_error(layout),
        };
        let ptr = non_null.as_ptr();
        unsafe { (&raw mut (*ptr).bitmap).write(bitmap) };
        let entries_ptr = unsafe { &raw mut (*ptr).entries as *mut T };
        let mut count = 0;
        for entry in entries {
            debug_assert!(count < len, "too many entries for bitmap");
            unsafe { entries_ptr.add(count).write(entry) };
            count += 1;
        }
        debug_assert_eq!(count, len, "too few entries for bitmap");
        Self(non_null)
    }

    pub fn bitmap(&self) -> Bitmap {
        unsafe { (*self.0.as_ptr()).bitmap }
    }

    pub fn entries(&self) -> &[T] {
        let len = self.bitmap().len();
        let ptr = unsafe { &raw const (*self.0.as_ptr()).entries }.cast::<T>();
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }

    pub fn entries_mut(&mut self) -> &mut [T] {
        let len = self.bitmap().len();
        let ptr = unsafe { &raw mut (*self.0.as_ptr()).entries }.cast::<T>();
        unsafe { std::slice::from_raw_parts_mut(ptr, len) }
    }

    fn layout(len: usize) -> Layout {
        Layout::new::<Bitmap>()
            .extend(Layout::array::<T>(len).unwrap())
            .unwrap()
            .0
            .pad_to_align()
    }
}

impl<T> SparseArray<T> {
    pub fn get(&self, bit: usize) -> Option<&T> {
        let bitmap = self.bitmap();
        bitmap.has(bit).then(|| &self.entries()[bitmap.rank(bit)])
    }
}

impl<T: Clone> SparseArray<T> {
    pub fn with_insert(&self, bit: usize, value: T) -> Self {
        let old_bitmap = self.bitmap();
        let new_bitmap = old_bitmap.with_set(bit);
        let idx = old_bitmap.rank(bit);
        let entries = self.entries();
        Self::new(
            new_bitmap,
            entries[..idx]
                .iter()
                .cloned()
                .chain(std::iter::once(value))
                .chain(entries[idx..].iter().cloned()),
        )
    }

    pub fn with_remove(&self, bit: usize) -> Self {
        let old_bitmap = self.bitmap();
        let new_bitmap = old_bitmap.with_clear(bit);
        let idx = old_bitmap.rank(bit);
        let entries = self.entries();
        Self::new(
            new_bitmap,
            entries[..idx]
                .iter()
                .cloned()
                .chain(entries[idx + 1..].iter().cloned()),
        )
    }

    pub fn with_replaced(&self, bit: usize, value: T) -> Self {
        let bitmap = self.bitmap();
        debug_assert!(bitmap.has(bit));
        let idx = bitmap.rank(bit);
        let entries = self.entries();
        Self::new(
            bitmap,
            entries[..idx]
                .iter()
                .cloned()
                .chain(std::iter::once(value))
                .chain(entries[idx + 1..].iter().cloned()),
        )
    }
}

impl<T> Default for SparseArray<T> {
    fn default() -> Self {
        Self::new(Bitmap::EMPTY, [])
    }
}

impl<T: Clone> Clone for SparseArray<T> {
    fn clone(&self) -> Self {
        Self::new(self.bitmap(), self.entries().iter().cloned())
    }
}

impl<T> Drop for SparseArray<T> {
    fn drop(&mut self) {
        if std::mem::needs_drop::<T>() {
            for entry in self.entries_mut() {
                unsafe { ptr::drop_in_place(entry as *mut T) };
            }
        }
        let layout = Self::layout(self.bitmap().len());
        unsafe { alloc::dealloc(self.0.as_ptr().cast(), layout) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmap_ops() {
        let b = Bitmap::EMPTY;
        assert!(!b.has(3));
        let b = b.with_set(3);
        assert!(b.has(3));
        assert_eq!(b.rank(3), 0);
        let b = b.with_set(7);
        assert_eq!(b.rank(7), 1); // one bit (3) is below 7
        let b = b.with_clear(3);
        assert!(!b.has(3));
        assert_eq!(b.rank(7), 0); // no bits below 7 now
    }

    #[test]
    fn get_entry() {
        let bitmap = Bitmap(0b1010); // bits 1 and 3
        let arr = SparseArray::new(bitmap, [10u32, 20u32]);
        assert_eq!(arr.get(1), Some(&10));
        assert_eq!(arr.get(3), Some(&20));
        assert_eq!(arr.get(0), None);
        assert_eq!(arr.get(2), None);
    }

    #[test]
    fn with_insert_and_remove() {
        let arr: SparseArray<u32> = SparseArray::default();
        let arr = arr.with_insert(5, 50);
        let arr = arr.with_insert(2, 20);
        // entries in physical order: bit 2 first, then bit 5
        assert_eq!(arr.entries(), &[20, 50]);
        assert_eq!(arr.get(2), Some(&20));
        assert_eq!(arr.get(5), Some(&50));

        let arr = arr.with_remove(2);
        assert_eq!(arr.entries(), &[50]);
        assert_eq!(arr.get(2), None);
        assert_eq!(arr.get(5), Some(&50));
    }

    #[test]
    fn with_replaced() {
        let bitmap = Bitmap(0b101); // bits 0 and 2
        let arr = SparseArray::new(bitmap, [1u32, 3u32]);
        let arr = arr.with_replaced(2, 99);
        assert_eq!(arr.get(0), Some(&1));
        assert_eq!(arr.get(2), Some(&99));
    }

    #[test]
    fn size_is_one_pointer() {
        assert_eq!(size_of::<SparseArray<u8>>(), size_of::<usize>());
        assert_eq!(size_of::<SparseArray<u128>>(), size_of::<usize>());
    }

    #[test]
    fn default_is_empty() {
        let arr = SparseArray::<u32>::default();
        assert_eq!(arr.bitmap(), Bitmap::EMPTY);
        assert!(arr.entries().is_empty());
    }

    #[test]
    fn new_and_entries() {
        let bitmap = Bitmap(0b1001); // bits 0 and 3 set => 2 entries
        let arr = SparseArray::new(bitmap, [10u32, 20u32]);
        assert_eq!(arr.bitmap(), bitmap);
        assert_eq!(arr.entries(), &[10, 20]);
    }

    #[test]
    fn clone_works() {
        let bitmap = Bitmap(0b111);
        let arr = SparseArray::new(bitmap, [1u32, 2u32, 3u32]);
        let cloned = arr.clone();
        assert_eq!(cloned.bitmap(), bitmap);
        assert_eq!(cloned.entries(), &[1, 2, 3]);
    }

    #[test]
    fn drop_is_called() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        let count = Arc::new(AtomicUsize::new(0));

        struct DropCounter(Arc<AtomicUsize>);
        impl Drop for DropCounter {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        {
            let bitmap = Bitmap(0b11);
            let _arr = SparseArray::new(
                bitmap,
                [
                    DropCounter(Arc::clone(&count)),
                    DropCounter(Arc::clone(&count)),
                ],
            );
        }

        assert_eq!(count.load(Ordering::Relaxed), 2);
    }
}
