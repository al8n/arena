use core::{
  fmt,
  mem::{self, MaybeUninit},
  ops,
  ptr::{self, NonNull},
  slice,
};

use crossbeam_utils::Backoff;

use crate::{common::*, ArenaOptions, Error};

#[cfg(all(feature = "memmap", not(target_family = "wasm")))]
use crate::{MmapOptions, OpenOptions};

#[allow(unused_imports)]
use std::boxed::Box;

mod backed;
use backed::*;

mod bytes;
pub use bytes::*;

mod object;
pub use object::*;

#[cfg(test)]
mod tests;

const OVERHEAD: usize = mem::size_of::<Header>();

#[derive(Debug)]
#[repr(C)]
struct Header {
  /// The sentinel node for the ordered free list.
  sentinel: AtomicU64,
  allocated: AtomicU32,
  min_segment_size: AtomicU32,
  discarded: AtomicU32,
}

impl Header {
  #[inline]
  fn new(size: u32, min_segment_size: u32) -> Self {
    Self {
      allocated: AtomicU32::new(size),
      sentinel: AtomicU64::new(encode_segment_node(u32::MAX, u32::MAX)),
      min_segment_size: AtomicU32::new(min_segment_size),
      discarded: AtomicU32::new(0),
    }
  }
}

struct Allocated {
  offset: u32,
  cap: u32,
}

/// Arena should be lock-free
pub struct Arena {
  write_data_ptr: NonNull<u8>,
  read_data_ptr: *const u8,
  ptr: *mut u8,
  data_offset: u32,
  max_retries: u8,
  inner: NonNull<Memory>,
  unify: bool,
  ro: bool,
  cap: u32,
}

impl fmt::Debug for Arena {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let header = self.header();
    let allocated = header.allocated.load(Ordering::Acquire);

    // Safety:
    // The ptr is always non-null, we only deallocate it when the ARENA is dropped.
    let data =
      unsafe { slice::from_raw_parts(self.read_data_ptr, (allocated - self.data_offset) as usize) };

    f.debug_struct("Arena")
      .field("cap", &self.cap)
      .field("header", header)
      .field("data", &data)
      .finish()
  }
}

impl Clone for Arena {
  fn clone(&self) -> Self {
    unsafe {
      let memory = self.inner.as_ref();

      let old_size = memory.refs.fetch_add(1, Ordering::Release);
      if old_size > usize::MAX >> 1 {
        abort();
      }

      // Safety:
      // The ptr is always non-null, and the data is only deallocated when the
      // last Arena is dropped.
      Self {
        write_data_ptr: self.write_data_ptr,
        read_data_ptr: self.read_data_ptr,
        max_retries: self.max_retries,
        ptr: self.ptr,
        data_offset: self.data_offset,
        ro: self.ro,
        inner: self.inner,
        unify: self.unify,
        cap: self.cap,
      }
    }
  }
}

impl Arena {
  /// Returns the number of bytes allocated by the ARENA.
  #[inline]
  pub fn size(&self) -> usize {
    self.header().allocated.load(Ordering::Acquire) as usize
  }

  /// Returns the capacity of the ARENA.
  #[inline]
  pub const fn capacity(&self) -> usize {
    self.cap as usize
  }

  /// Returns the number of bytes remaining bytes can be allocated by the ARENA.
  #[inline]
  pub fn remaining(&self) -> usize {
    (self.cap as usize).saturating_sub(self.size())
  }

  /// Returns the number of references to the ARENA.
  #[inline]
  pub fn refs(&self) -> usize {
    unsafe { self.inner.as_ref().refs.load(Ordering::Acquire) }
  }

  /// Returns the number of bytes discarded by the ARENA.
  #[inline]
  pub fn discarded(&self) -> usize {
    self.header().discarded.load(Ordering::Acquire) as usize
  }

  /// Forcelly increases the discarded bytes.
  #[inline]
  pub fn increase_discarded(&self, size: usize) {
    self
      .header()
      .discarded
      .fetch_add(size as u32, Ordering::Release);
  }

  /// Returns the minimum segment size of the ARENA.
  #[inline]
  pub fn minimum_segment_size(&self) -> usize {
    self.header().min_segment_size.load(Ordering::Acquire) as usize
  }

  /// Sets the minimum segment size of the ARENA.
  #[inline]
  pub fn set_minimum_segment_size(&self, size: usize) {
    self
      .header()
      .min_segment_size
      .store(size as u32, Ordering::Release);
  }

  /// Returns the data offset of the ARENA. The offset is the end of the ARENA header.
  #[inline]
  pub const fn data_offset(&self) -> usize {
    self.data_offset as usize
  }

  /// Returns the data section of the ARENA as a byte slice, header is not included.
  #[inline]
  pub fn data(&self) -> &[u8] {
    unsafe {
      let ptr = self.ptr.add(self.data_offset as usize);
      let allocated = self.header().allocated.load(Ordering::Acquire);
      slice::from_raw_parts(ptr, (allocated - self.data_offset) as usize)
    }
  }

  /// Returns the data section of the ARENA as a mutable byte slice, header is included.
  #[inline]
  pub fn data_with_header(&self) -> &[u8] {
    unsafe {
      let ptr = self.ptr.add(self.data_offset as usize);
      let allocated = self.header().allocated.load(Ordering::Acquire);
      slice::from_raw_parts(ptr, allocated as usize)
    }
  }

  #[inline]
  fn header(&self) -> &Header {
    // Safety:
    // The inner is always non-null, we only deallocate it when the ARENA is dropped.
    unsafe { (*self.inner.as_ptr()).header() }
  }
}

unsafe impl Send for Arena {}
unsafe impl Sync for Arena {}

impl Arena {
  /// Creates a new ARENA with the given capacity,
  #[inline]
  pub fn new(opts: ArenaOptions) -> Self {
    let memory = Memory::new_vec(
      opts.capacity(),
      opts.maximum_alignment(),
      opts.minimum_segment_size(),
      opts.unify(),
    );
    Self::new_in(memory, opts.maximum_retries(), opts.unify(), false)
  }

  /// Creates a new ARENA backed by a mmap with the given capacity.
  #[cfg(all(feature = "memmap", not(target_family = "wasm")))]
  #[inline]
  pub fn map_mut<P: AsRef<std::path::Path>>(
    path: P,
    opts: ArenaOptions,
    open_options: OpenOptions,
    mmap_options: MmapOptions,
  ) -> std::io::Result<Self> {
    Memory::map_mut(
      path,
      open_options,
      mmap_options,
      opts.maximum_alignment(),
      opts.minimum_segment_size(),
    )
    .map(|memory| Self::new_in(memory, opts.maximum_retries(), true, false))
  }

  /// Creates a new read only ARENA backed by a mmap with the given capacity.
  #[cfg(all(feature = "memmap", not(target_family = "wasm")))]
  #[inline]
  pub fn map<P: AsRef<std::path::Path>>(
    path: P,
    open_options: OpenOptions,
    mmap_options: MmapOptions,
  ) -> std::io::Result<Self> {
    Memory::map(path, open_options, mmap_options).map(|memory| Self::new_in(memory, 0, true, true))
  }

  /// Creates a new ARENA backed by an anonymous mmap with the given capacity.
  #[cfg(all(feature = "memmap", not(target_family = "wasm")))]
  #[inline]
  pub fn map_anon(opts: ArenaOptions, mmap_options: MmapOptions) -> std::io::Result<Self> {
    Memory::map_anon(
      mmap_options,
      opts.maximum_alignment(),
      opts.minimum_segment_size(),
      opts.unify(),
    )
    .map(|memory| Self::new_in(memory, opts.maximum_retries(), opts.unify(), false))
  }

  /// Allocates an owned slice of memory in the ARENA.
  ///
  /// The cost of this method is an extra atomic operation, compared to [`alloc_bytes`](Self::alloc_bytes).
  #[inline]
  pub fn alloc_bytes_owned(&self, size: u32) -> Result<BytesMut, Error> {
    self.alloc_bytes(size).map(|mut b| b.to_owned())
  }

  /// Allocates a slice of memory in the ARENA.
  ///
  /// The [`BytesRefMut`] is zeroed out.
  ///
  /// If you want a [`BytesMut`], see [`alloc_bytes_owned`](Self::alloc_bytes_owned).
  #[inline]
  pub fn alloc_bytes(&self, size: u32) -> Result<BytesRefMut, Error> {
    self.alloc_bytes_in(size).map(|a| match a {
      None => BytesRefMut::null(self),
      Some(allocated) => unsafe { BytesRefMut::new(self, allocated.cap, allocated.offset) },
    })
  }

  /// Flushes the memory-mapped file to disk.
  #[cfg(all(feature = "memmap", not(target_family = "wasm")))]
  pub fn flush(&self) -> std::io::Result<()> {
    unsafe { self.inner.as_ref().flush() }
  }

  /// Flushes the memory-mapped file to disk asynchronously.
  #[cfg(all(feature = "memmap", not(target_family = "wasm")))]
  pub fn flush_async(&self) -> std::io::Result<()> {
    unsafe { self.inner.as_ref().flush_async() }
  }

  /// Allocates a `T` in the ARENA.
  ///
  /// # Safety
  ///
  /// - If `T` needs to be dropped and callers invoke [`RefMut::detach`],
  ///   then the caller must ensure that the `T` is dropped before the ARENA is dropped.
  ///   Otherwise, it will lead to memory leaks.
  ///
  /// - If this is file backed ARENA, then `T` must be recoverable from bytes.
  ///   1. Types require allocation are not recoverable.
  ///   2. Pointers are not recoverable, like `*const T`, `*mut T`, `NonNull` and any structs contains pointers,
  ///      although those types are on stack, but they cannot be recovered, when reopens the file.
  ///
  /// # Examples
  ///
  /// ## Memory leak
  ///
  /// The following example demonstrates the memory leak when the `T` is a heap allocated type and detached.
  ///
  /// ```ignore
  ///
  /// let ARENA = Arena::new(ArenaOptions::new());
  ///
  /// {
  ///   let mut data = ARENA.alloc::<Vec<u8>>().unwrap();
  ///   data.detach();
  ///   data.write(vec![1, 2, 3]);
  /// }
  ///
  /// drop(ARENA); // memory leak, the `Vec<u8>` is not dropped.
  /// ```
  ///
  /// ## Undefined behavior
  ///
  /// The following example demonstrates the undefined behavior when the `T` is not recoverable.
  ///
  /// ```ignore
  ///
  /// struct TypeOnHeap {
  ///   data: Vec<u8>,
  /// }
  ///
  /// let ARENA = Arena::map_mut("path/to/file", ArenaOptions::new(), OpenOptions::create_new(Some(1000)).read(true).write(true), MmapOptions::default()).unwrap();
  ///
  /// let mut data = ARENA.alloc::<TypeOnHeap>().unwrap();
  /// data.detach();
  /// data.write(TypeOnHeap { data: vec![1, 2, 3] });
  /// let offset = data.offset();
  /// drop(ARENA);
  ///
  /// // reopen the file
  /// let ARENA = Arena::map("path/to/file", OpenOptions::read(true), MmapOptions::default()).unwrap();
  ///
  /// let foo = &*ARENA.get_aligned_pointer::<TypeOnHeap>(offset as usize);
  /// let b = foo.data[1]; // undefined behavior, the `data`'s pointer stored in the file is not valid anymore.
  /// ```
  ///
  /// ## Good practice
  ///
  /// Some examples about how to use this method correctly.
  ///
  /// ### Heap allocated type with carefull memory management
  ///
  /// ```ignore
  /// let ARENA = Arena::new(ArenaOptions::new());
  ///
  /// // Do not invoke detach, so when the data is dropped, the drop logic will be handled by the ARENA.
  /// // automatically.
  /// {
  ///   let mut data = ARENA.alloc::<Vec<u8>>().unwrap();
  ///   data.write(vec![1, 2, 3]);
  /// }
  ///
  ///
  /// let mut detached_data = ARENA.alloc::<Vec<u8>>().unwrap();
  /// detached_data.detach();
  /// detached_data.write(vec![4, 5, 6]);
  ///
  /// // some other logic
  ///
  /// core::ptr::drop_in_place(detached_data.as_mut()); // drop the `Vec` manually.
  ///
  /// drop(ARENA); // it is safe, the `Vec` is already dropped.
  /// ```
  ///
  /// ### Recoverable type with file backed ARENA
  ///
  /// ```ignore
  ///
  /// struct Recoverable {
  ///   field1: u64,
  ///   field2: AtomicU32,
  /// }
  ///
  /// let ARENA = Arena::map_mut("path/to/file", ArenaOptions::new(), OpenOptions::create_new(Some(1000)).read(true).write(true), MmapOptions::default()).unwrap();
  ///
  /// let mut data = ARENA.alloc::<Recoverable>().unwrap();
  /// data.write(Recoverable { field1: 10, field2: AtomicU32::new(20) });
  ///
  /// let offset = data.offset();
  /// drop(ARENA);
  ///
  /// // reopen the file
  /// let ARENA = Arena::map("path/to/file", OpenOptions::read(true), MmapOptions::default()).unwrap();
  ///
  /// let foo = &*ARENA.get_aligned_pointer::<Recoverable>(offset as usize);
  ///
  /// assert_eq!(foo.field1, 10);
  /// assert_eq!(foo.field2.load(Ordering::Acquire), 20);
  /// ```
  #[inline]
  pub unsafe fn alloc<T>(&self) -> Result<RefMut<'_, T>, Error> {
    if mem::size_of::<T>() == 0 {
      return Ok(RefMut::new_zst(self));
    }

    let allocated = self
      .alloc_in::<T>()?
      .expect("allocated size is not zero, but get None");
    let ptr = unsafe { self.get_aligned_pointer_mut::<T>(allocated.offset as usize) };
    if mem::needs_drop::<T>() {
      unsafe {
        let ptr: *mut MaybeUninit<T> = ptr.as_ptr().cast();
        ptr::write(ptr, MaybeUninit::uninit());

        Ok(RefMut::new(
          ptr::read(ptr),
          allocated.offset,
          allocated.cap as usize,
          self,
        ))
      }
    } else {
      Ok(RefMut::new_inline(
        ptr,
        allocated.offset,
        allocated.cap as usize,
        self,
      ))
    }
  }

  /// Clear the ARENA.
  ///
  /// # Safety
  /// - The current pointers get from the ARENA cannot be used anymore after calling this method.
  /// - This method is not thread-safe.
  ///
  /// # Examples
  ///
  /// Undefine behavior:
  ///
  /// ```ignore
  /// let mut data = ARENA.alloc::<Vec<u8>>().unwrap();
  ///
  /// ARENA.clear();
  ///
  /// data.write(vec![1, 2, 3]); // undefined behavior
  /// ```
  pub unsafe fn clear(&self) -> Result<(), Error> {
    if self.ro {
      return Err(Error::ReadOnly);
    }

    let header = self.header();
    header.allocated.store(self.data_offset, Ordering::Release);
    header
      .sentinel
      .store(encode_segment_node(u32::MAX, u32::MAX), Ordering::Release);
    header.discarded.store(0, Ordering::Release);
    // Safety:
    // 1. pointer is well aligned
    // 2. cap is in bounds
    unsafe {
      ptr::write_bytes(self.write_data_ptr.as_ptr(), 0, self.cap as usize);
    }
    Ok(())
  }

  /// Returns a bytes slice from the ARENA.
  ///
  /// # Safety
  /// - `offset..offset + size` must be allocated memory.
  /// - `offset` must be less than the capacity of the ARENA.
  /// - `size` must be less than the capacity of the ARENA.
  /// - `offset + size` must be less than the capacity of the ARENA.
  #[inline]
  pub const unsafe fn get_bytes(&self, offset: usize, size: usize) -> &[u8] {
    if offset == 0 {
      return &[];
    }

    let ptr = self.get_pointer(offset);
    slice::from_raw_parts(ptr, size)
  }

  /// Returns a mutable bytes slice from the ARENA.
  /// If the ARENA is read-only, then this method will return an empty slice.
  ///
  /// # Safety
  /// - `offset..offset + size` must be allocated memory.
  /// - `offset` must be less than the capacity of the ARENA.
  /// - `size` must be less than the capacity of the ARENA.
  /// - `offset + size` must be less than the capacity of the ARENA.
  ///
  /// # Panic
  /// - If the ARENA is read-only, then this method will panic.
  #[allow(clippy::mut_from_ref)]
  #[inline]
  pub unsafe fn get_bytes_mut(&self, offset: usize, size: usize) -> &mut [u8] {
    assert!(!self.ro, "ARENA is read-only");

    if offset == 0 {
      return &mut [];
    }

    let ptr = self.get_pointer_mut(offset);
    if ptr.is_null() {
      return &mut [];
    }

    slice::from_raw_parts_mut(ptr, size)
  }

  /// Returns a pointer to the memory at the given offset.
  ///
  /// # Safety
  /// - `offset` must be less than the capacity of the ARENA.
  #[inline]
  pub const unsafe fn get_pointer(&self, offset: usize) -> *const u8 {
    if offset == 0 {
      return self.ptr;
    }
    self.read_data_ptr.add(offset)
  }

  /// Returns a pointer to the memory at the given offset.
  /// If the ARENA is read-only, then this method will return a null pointer.
  ///
  /// # Safety
  /// - `offset` must be less than the capacity of the ARENA.
  ///
  /// # Panic
  /// - If the ARENA is read-only, then this method will panic.
  #[inline]
  pub unsafe fn get_pointer_mut(&self, offset: usize) -> *mut u8 {
    assert!(!self.ro, "ARENA is read-only");

    if offset == 0 {
      return self.ptr;
    }
    let ptr = self.write_data_ptr.as_ptr();
    if ptr.is_null() {
      return ptr::null_mut();
    }
    ptr.add(offset)
  }

  /// Returns an aligned pointer to the memory at the given offset.
  ///
  /// # Safety
  /// - `offset..offset + mem::size_of::<T>() + padding` must be allocated memory.
  /// - `offset` must be less than the capacity of the ARENA.
  #[inline]
  pub unsafe fn get_aligned_pointer<T>(&self, offset: usize) -> *const T {
    if offset == 0 {
      return ptr::null();
    }

    let ptr = self.read_data_ptr.add(offset);
    let aligned_offset = ptr.align_offset(mem::align_of::<T>());
    ptr.add(aligned_offset).cast()
  }

  /// Returns an aligned pointer to the memory at the given offset.
  /// If the ARENA is read-only, then this method will return a null pointer.
  ///
  /// # Safety
  /// - `offset..offset + mem::size_of::<T>() + padding` must be allocated memory.
  /// - `offset` must be less than the capacity of the ARENA.
  ///
  /// # Panic
  /// - If the ARENA is read-only, then this method will panic.
  #[inline]
  pub unsafe fn get_aligned_pointer_mut<T>(&self, offset: usize) -> NonNull<T> {
    assert!(!self.ro, "ARENA is read-only");

    if offset == 0 {
      return NonNull::dangling();
    }

    let ptr = self.write_data_ptr.as_ptr();
    if ptr.is_null() {
      return NonNull::dangling();
    }

    let ptr = ptr.add(offset);
    let aligned_offset = ptr.align_offset(mem::align_of::<T>());
    NonNull::new_unchecked(ptr.add(aligned_offset).cast())
  }

  /// Returns the offset to the start of the ARENA.
  ///
  /// # Safety
  /// - `ptr` must be allocated by this ARENA.
  #[inline]
  pub unsafe fn offset(&self, ptr: *mut u8) -> usize {
    let offset = ptr.offset_from(self.write_data_ptr.as_ptr());
    offset as usize
  }

  fn alloc_bytes_in(&self, size: u32) -> Result<Option<Allocated>, Error> {
    if self.ro {
      return Err(Error::ReadOnly);
    }

    if size == 0 {
      return Ok(None);
    }

    let header = self.header();
    let mut allocated = header.allocated.load(Ordering::Acquire);

    loop {
      let want = allocated + size;
      if want > self.cap {
        break;
      }

      match header.allocated.compare_exchange_weak(
        allocated,
        want,
        Ordering::SeqCst,
        Ordering::Acquire,
      ) {
        Ok(offset) => return Ok(Some(Allocated { offset, cap: size })),
        Err(x) => allocated = x,
      }
    }

    // allocate through slow path
    let mut i = 0;

    loop {
      match self.alloc_slow_path(size) {
        Ok(bytes) => return Ok(bytes),
        Err(e) => {
          if i == self.max_retries - 1 {
            return Err(e);
          }
        }
      }
      i += 1;
    }
  }

  /// It is like a pop operation, we will always allocate from the largest segment.
  fn alloc_slow_path(&self, size: u32) -> Result<Option<Allocated>, Error> {
    if self.ro {
      return Err(Error::ReadOnly);
    }

    let backoff = Backoff::new();
    let header = self.header();

    loop {
      let head = header.sentinel.load(Ordering::Acquire);
      let (next, node_size) = decode_segment_node(head);
      // free list is empty
      if next == u32::MAX && node_size == u32::MAX {
        return Err(Error::InsufficientSpace {
          requested: size,
          available: self.remaining() as u32,
        });
      }

      if node_size == 0 {
        // The current head is removed from the list, wait other thread to make progress.
        backoff.snooze();
        continue;
      }

      // The larget segment does not have enough space to allocate, so just return err.
      if size > node_size {
        return Err(Error::InsufficientSpace {
          requested: size,
          available: node_size,
        });
      }

      // CAS to remove the current
      let removed_head = encode_segment_node(next, 0);
      if header
        .sentinel
        .compare_exchange_weak(head, removed_head, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
      {
        // wait other thread to make progress.
        backoff.snooze();
        continue;
      }

      // We have successfully mark the head is removed, then we need to let head node's next point to the next node.
      let next_node = unsafe { self.get_segment_node(next) };
      let next_node_val = next_node.load(Ordering::Acquire);

      match header.sentinel.compare_exchange(
        removed_head,
        next_node_val,
        Ordering::AcqRel,
        Ordering::Relaxed,
      ) {
        Ok(_) => {
          // We have successfully remove the head node from the list.
          // Then we can allocate the memory.
          // give back the remaining memory to the free list.
          self.dealloc(next + size, node_size - size);
          return Ok(Some(Allocated {
            offset: next,
            cap: size,
          }));
        }
        Err(current_sentinel) => {
          let (_, size) = decode_segment_node(current_sentinel);
          if size == 0 {
            // The current head is removed from the list, wait other thread to make progress.
            backoff.snooze();
            continue;
          }

          backoff.spin();
        }
      }
    }
  }

  fn alloc_in<T>(&self) -> Result<Option<Allocated>, Error> {
    if self.ro {
      return Err(Error::ReadOnly);
    }

    if mem::size_of::<T>() == 0 {
      return Ok(None);
    }

    let header = self.header();
    let mut allocated = header.allocated.load(Ordering::Acquire);

    unsafe {
      loop {
        let ptr = self.get_pointer(allocated as usize);
        let aligned_offset = ptr.align_offset(mem::align_of::<T>()) as u32;
        let size = aligned_offset + mem::size_of::<T>() as u32;
        let want = allocated + size;
        if want > self.cap {
          break;
        }

        match header.allocated.compare_exchange_weak(
          allocated,
          want,
          Ordering::SeqCst,
          Ordering::Acquire,
        ) {
          Ok(offset) => return Ok(Some(Allocated { offset, cap: size })),
          Err(x) => allocated = x,
        }
      }
    }

    // allocate through slow path
    let mut i = 0;
    loop {
      match self.alloc_slow_path(Self::pad::<T>() as u32) {
        Ok(bytes) => return Ok(bytes),
        Err(e) => {
          if i == self.max_retries - 1 {
            return Err(e);
          }
        }
      }
      i += 1;
    }
  }

  fn dealloc(&self, offset: u32, size: u32) {
    // check if we have enough space to allocate a new segment in this segment.
    if !self.validate_segment(offset, size) {
      self.discard(size);
      return;
    }

    let backoff = Backoff::new();

    unsafe {
      let ptr = self.write_data_ptr.as_ptr().add(offset as usize);

      // clear the memory
      ptr::write_bytes(ptr, 0, size as usize);
      let header = self.header();

      loop {
        let (prev, next) = self.find_free_list_position(size);

        let prev_node = prev
          .map(|p| self.get_segment_node(p))
          .unwrap_or(&header.sentinel);
        let next_node_offset = next.unwrap_or(u32::MAX);

        self.write_segment_node(next_node_offset, offset, size);

        // CAS prev_node's next points to the new_node
        let prev_node_val = prev_node.load(Ordering::Acquire);
        let (_, prev_node_size) = decode_segment_node(prev_node_val);

        // the prev is removed from the list, then we need to refind the position.
        if prev_node_size == 0 {
          // wait other thread to make progress.
          backoff.snooze();
          continue;
        }

        match prev_node.compare_exchange(
          prev_node_val,
          encode_segment_node(offset, size),
          Ordering::AcqRel,
          Ordering::Relaxed,
        ) {
          Ok(_) => break,
          Err(current_prev) => {
            let (_, size) = decode_segment_node(current_prev);
            // the prev is removed from the list, then we need to refind the position.
            if size == 0 {
              // wait other thread to make progress.
              backoff.snooze();
              continue;
            }

            backoff.spin();
          }
        }
      }
    }
  }

  /// Returns `true` if this offset and size is valid for a segment node.
  #[inline]
  fn validate_segment(&self, offset: u32, size: u32) -> bool {
    unsafe {
      let ptr = self.write_data_ptr.as_ptr().add(offset as usize);
      let aligned_offset = ptr.align_offset(mem::align_of::<AtomicU64>());
      let want = aligned_offset + mem::size_of::<AtomicU64>() + mem::size_of::<u32>();
      if want >= size as usize {
        return false;
      }

      if size < self.header().min_segment_size.load(Ordering::Acquire) {
        return false;
      }

      true
    }
  }

  fn find_free_list_position(&self, val: u32) -> (Option<u32>, Option<u32>) {
    let header = self.header();
    let mut current = &header.sentinel;

    let mut prev = 0;
    let backoff = Backoff::new();
    loop {
      let current_node = current.load(Ordering::Acquire);
      let (current_next, current_node_size) = decode_segment_node(current_node);

      // we reach the tail of the list
      if current_next == u32::MAX {
        // the list is empty
        if prev == 0 {
          return (None, None);
        }

        return (Some(prev), None);
      }

      // the current is marked as removed
      if current_node_size == 0 {
        // wait other thread to remove the node.
        backoff.snooze();
        continue;
      }

      // the size is smaller than or equal to the val
      // then the value should be inserted before the current node
      if val >= current_node_size {
        if prev == 0 {
          return (None, Some(current_next));
        }

        return (Some(prev), Some(current_next));
      }

      let next = unsafe { self.get_segment_node(current_next) };

      prev = current_next;
      current = next;
      backoff.spin();
    }
  }

  #[inline]
  fn discard(&self, size: u32) {
    let header = self.header();
    header.discarded.fetch_add(size, Ordering::Release);
  }

  unsafe fn get_segment_node(&self, offset: u32) -> &AtomicU64 {
    let ptr = self.read_data_ptr.add(offset as usize);
    let aligned_offset = ptr.align_offset(mem::align_of::<AtomicU64>());
    let ptr = ptr.add(aligned_offset);
    &*(ptr as *const _)
  }

  unsafe fn write_segment_node(&self, next: u32, offset: u32, size: u32) -> u32 {
    let ptr = self.write_data_ptr.as_ptr().add(offset as usize);
    let aligned_offset = ptr.align_offset(mem::align_of::<AtomicU64>());
    let ptr = ptr.add(aligned_offset);
    let node = ptr as *mut AtomicU64;
    let node = &mut *node;
    node.store(encode_segment_node(next, size), Ordering::Release);
    offset
  }

  #[inline]
  fn new_in(mut memory: Memory, max_retries: u8, unify: bool, ro: bool) -> Self {
    // Safety:
    // The ptr is always non-null, we just initialized it.
    // And this ptr is only deallocated when the ARENA is dropped.
    let read_data_ptr = memory.as_ptr();
    let ptr = memory.null_mut();
    let write_data_ptr = memory
      .as_mut_ptr()
      .map(|p| unsafe { NonNull::new_unchecked(p) })
      .unwrap_or_else(NonNull::dangling);

    Self {
      cap: memory.cap(),
      write_data_ptr,
      read_data_ptr,
      unify,
      ptr,
      ro,
      max_retries,
      data_offset: memory.data_offset as u32,
      inner: unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(memory)) as _) },
    }
  }

  #[inline]
  fn pad<T>() -> usize {
    let size = mem::size_of::<T>();
    let align = mem::align_of::<T>();
    size + align - 1
  }
}

impl Drop for Arena {
  fn drop(&mut self) {
    unsafe {
      let memory = self.inner.as_mut();
      // `Memory` storage... follow the drop steps from Arc.
      if memory.refs.fetch_sub(1, Ordering::Release) != 1 {
        return;
      }

      // This fence is needed to prevent reordering of use of the data and
      // deletion of the data.  Because it is marked `Release`, the decreasing
      // of the reference count synchronizes with this `Acquire` fence. This
      // means that use of the data happens before decreasing the reference
      // count, which happens before this fence, which happens before the
      // deletion of the data.
      //
      // As explained in the [Boost documentation][1],
      //
      // > It is important to enforce any possible access to the object in one
      // > thread (through an existing reference) to *happen before* deleting
      // > the object in a different thread. This is achieved by a "release"
      // > operation after dropping a reference (any access to the object
      // > through this reference must obviously happened before), and an
      // > "acquire" operation before deleting the object.
      //
      // [1]: (www.boost.org/doc/libs/1_55_0/doc/html/atomic/usage_examples.html)
      //
      // Thread sanitizer does not support atomic fences. Use an atomic load
      // instead.
      memory.refs.load(Ordering::Acquire);
      // Drop the data
      let mut memory = Box::from_raw(memory);

      // Relaxed is enough here as we're in a drop, no one else can
      // access this memory anymore.
      memory.unmount();
    }
  }
}

#[cfg(all(feature = "memmap", not(target_family = "wasm")))]
fn invalid_data<E: std::error::Error + Send + Sync + 'static>(e: E) -> std::io::Error {
  std::io::Error::new(std::io::ErrorKind::InvalidData, e)
}

#[inline]
const fn decode_segment_node(val: u64) -> (u32, u32) {
  ((val >> 32) as u32, val as u32)
}

#[inline]
const fn encode_segment_node(next: u32, size: u32) -> u64 {
  ((next as u64) << 32) | size as u64
}

#[inline(never)]
#[cold]
fn abort() -> ! {
  #[cfg(feature = "std")]
  {
    std::process::abort()
  }

  #[cfg(not(feature = "std"))]
  {
    struct Abort;
    impl Drop for Abort {
      fn drop(&mut self) {
        panic!();
      }
    }
    let _a = Abort;
    panic!("abort");
  }
}
