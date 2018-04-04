
/// rewrites calls to either `liballoc_system` or `liballoc_jemalloc`.
/// TODO replace these aborts with writes to a AQL queue to the host
/// to request memory.

use std::intrinsics::abort;

use hsa_core::kernel_info::kernel_info_for;

use rustc::ty::item_path::{with_forced_absolute_paths};

use super::{Pass, PassType};

#[inline(never)]
fn __rust_alloc(size: usize, align: usize, err: *mut u8) -> *mut u8 {
  unsafe { abort() };
}

#[inline(never)]
fn __rust_oom(err: *const u8) -> ! {
  unsafe { abort() };
}
#[inline(never)]
fn __rust_dealloc(ptr: *mut u8, size: usize, align: usize) {
  unsafe { abort() };
}
#[inline(never)]
fn __rust_usable_size(layout: *const u8,
                      min: *mut usize,
                      max: *mut usize) {
  unsafe { abort() };
}
#[inline(never)]
fn __rust_realloc(ptr: *mut u8,
                  old_size: usize,
                  old_align: usize,
                  new_size: usize,
                  new_align: usize,
                  err: *mut u8) -> *mut u8 {
  unsafe { abort() };
}

#[inline(never)]
fn __rust_alloc_zeroed(size: usize, align: usize, err: *mut u8) -> *mut u8 {
  unsafe { abort() };
}

#[inline(never)]
fn __rust_alloc_excess(size: usize,
                       align: usize,
                       excess: *mut usize,
                       err: *mut u8) -> *mut u8 {
  unsafe { abort() };
}

#[inline(never)]
fn __rust_realloc_excess(ptr: *mut u8,
                         old_size: usize,
                         old_align: usize,
                         new_size: usize,
                         new_align: usize,
                         excess: *mut usize,
                         err: *mut u8) -> *mut u8 {
  unsafe { abort() };
}

#[inline(never)]
fn __rust_grow_in_place(ptr: *mut u8,
                        old_size: usize,
                        old_align: usize,
                        new_size: usize,
                        new_align: usize) -> u8 {
  unsafe { abort() };
}

#[inline(never)]
fn __rust_shrink_in_place(ptr: *mut u8,
                          old_size: usize,
                          old_align: usize,
                          new_size: usize,
                          new_align: usize) -> u8 {
  unsafe { abort() };
}

#[derive(Clone, Debug)]
pub struct AllocPass;

impl Pass for AllocPass {
  fn pass_type(&self) -> PassType {
    PassType::Replacer(|tcx, def_id| {
      let path = with_forced_absolute_paths(|| tcx.item_path_str(def_id) );
      let info = match &path[..] {
        "alloc::heap::::__rust_alloc" => {
          kernel_info_for(&__rust_alloc)
        },
        "alloc::heap::::__rust_alloc_zeroed" => {
          kernel_info_for(&__rust_alloc_zeroed)
        },
        "alloc::heap::::__rust_realloc" => {
          kernel_info_for(&__rust_realloc)
        },
        "alloc::heap::::__rust_oom" => {
          kernel_info_for(&__rust_oom)
        },
        "alloc::heap::::__rust_usable_size" => {
          kernel_info_for(&__rust_usable_size)
        },
        "alloc::heap::::__rust_dealloc" => {
          kernel_info_for(&__rust_dealloc)
        },
        "alloc::heap::::__rust_alloc_excess" => {
          kernel_info_for(&__rust_alloc_excess)
        },
        "alloc::heap::::__rust_realloc_excess" => {
          kernel_info_for(&__rust_realloc_excess)
        },
        "alloc::heap::::__rust_grow_in_place" => {
          kernel_info_for(&__rust_grow_in_place)
        },
        "alloc::heap::::__rust_shrink_in_place" => {
          kernel_info_for(&__rust_shrink_in_place)
        },
        _ => { return None; },
      };

      Some(tcx.as_def_id(info.id).unwrap())
    })
  }
}
