//! Where the app's memory comes from on macOS.
//!
//! The pictures of pages are tens of megabytes each, and come and go as the
//! document is scrolled. The system's allocator keeps such blocks when they
//! are freed, to hand out again, and counts them against the app for as long
//! as it runs (asking it to let go of them does nothing). So blocks this big
//! are taken from the system directly and given straight back: the app then
//! takes the memory its pages take, not what they once took.
//!
//! (Elsewhere the allocator already does this for blocks this size.)

use std::alloc::{GlobalAlloc, Layout, System};
use std::ffi::{c_int, c_void};

/// Blocks at least this big are mapped. Smaller ones are the allocator's.
const BIG: usize = 1 << 20;
/// What mappings are rounded up to: a whole number of pages on any Mac.
const GRAIN: usize = 1 << 14;

const PROT_READ_WRITE: c_int = 0x1 | 0x2;
const MAP_PRIVATE_ANON: c_int = 0x0002 | 0x1000;

unsafe extern "C" {
    fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, offset: i64) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
}

pub struct Returning;

fn mapped(layout: Layout) -> bool {
    layout.size() >= BIG && layout.align() <= GRAIN
}

fn rounded(size: usize) -> usize {
    size.div_ceil(GRAIN) * GRAIN
}

// SAFETY: every block is either the system allocator's or a mapping of its
// own, and which it is follows from its layout alone, which the caller gives
// back unchanged. Mappings start on a page, are zeroed, and belong to nothing else.
unsafe impl GlobalAlloc for Returning {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !mapped(layout) {
            return unsafe { System.alloc(layout) };
        }
        let block = unsafe { mmap(std::ptr::null_mut(), rounded(layout.size()), PROT_READ_WRITE, MAP_PRIVATE_ANON, -1, 0) };
        // (MAP_FAILED is all ones.)
        if block as isize == -1 { std::ptr::null_mut() } else { block.cast() }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if mapped(layout) { unsafe { self.alloc(layout) } } else { unsafe { System.alloc_zeroed(layout) } }
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        if mapped(layout) {
            unsafe { munmap(block.cast(), rounded(layout.size())) };
        } else {
            unsafe { System.dealloc(block, layout) }
        }
    }

    unsafe fn realloc(&self, block: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: the caller promises `size` is valid for this alignment.
        let wanted = unsafe { Layout::from_size_align_unchecked(size, layout.align()) };
        match (mapped(layout), mapped(wanted)) {
            (false, false) => unsafe { System.realloc(block, layout, size) },
            // Shrinking a mapping: the pages past its new end go back.
            (true, true) if rounded(size) <= rounded(layout.size()) => {
                let (keep, had) = (rounded(size), rounded(layout.size()));
                if keep < had {
                    unsafe { munmap(block.add(keep).cast(), had - keep) };
                }
                block
            }
            _ => {
                let moved = unsafe { self.alloc(wanted) };
                if !moved.is_null() {
                    unsafe {
                        std::ptr::copy_nonoverlapping(block, moved, layout.size().min(size));
                        self.dealloc(block, layout);
                    }
                }
                moved
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn big_blocks_are_mapped_and_keep_what_is_in_them() {
        // (This is the allocator the tests run with, too.)
        let mut bytes: Vec<u8> = Vec::new();
        // Growing from a small block through to big ones, a byte at a time and in leaps.
        for i in 0..3 * BIG {
            bytes.push((i % 251) as u8);
        }
        bytes.resize(9 * BIG + 7, 7);
        assert!(bytes[..3 * BIG].iter().enumerate().all(|(i, b)| *b == (i % 251) as u8));
        assert!(bytes[3 * BIG..].iter().all(|b| *b == 7));
        // Shrinking: within a mapping, and back to a small block.
        bytes.truncate(2 * BIG + 1);
        bytes.shrink_to_fit();
        assert!(bytes.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8));
        bytes.truncate(1000);
        bytes.shrink_to_fit();
        assert!(bytes.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8));

        // Zeroed blocks are zero, and a block's start suits what it holds.
        let zeroed = vec![0u64; BIG];
        assert!(zeroed.iter().all(|v| *v == 0) && zeroed.as_ptr() as usize % GRAIN == 0);
        let layout = Layout::from_size_align(2 * BIG, 4 * GRAIN).unwrap();
        // SAFETY: allocated and freed with the same layout.
        unsafe {
            let wide = Returning.alloc(layout);
            assert!(!wide.is_null() && wide as usize % (4 * GRAIN) == 0, "more than a page's alignment is the system's to give");
            Returning.dealloc(wide, layout);
        }
    }
}
