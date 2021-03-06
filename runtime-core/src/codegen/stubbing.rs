
/// Some functions, like `std::panicking::rust_panic_hook`,
/// we need to override with an aborting stub. `panic!`
/// will likely never be supported in shaders, and probably won't
/// receive support in kernels.
/// TODO info about which stub caused an abort.
/// TODO it would be nice to be able to map DefId -> KernelId instead of
/// String -> KernelId. Need a way to translate the absolute path into a
/// DefId.

use std::geobacter::kernel::*;

use rustc_data_structures::fx::{FxHashMap};
use rustc_geobacter::TyCtxtKernelInstance;
use rustc_hir::def_id::DefId;
use rustc_middle::ty::TyCtxt;

pub struct Stubber {
  /// Are we stubbing the "builtin" stubs? See `self::stubs`.
  builtins: bool,
  stubs: FxHashMap<String, KernelInstanceRef<'static>>,
}

impl Stubber {
  pub fn stub_def_id<'tcx>(&self, tcx: TyCtxt<'tcx>, did: DefId) -> DefId {
    use self::stubs::*;

    let path = tcx.def_path_str(did);

    if path.starts_with("geobacter_runtime_core::codegen::stubbing") {
      // this is one of our stubs.
      return did;
    }

    let convert_ki = |ki: KernelInstanceRef<'_>| {
      let stub_instance = tcx.expect_instance(ki);
      trace!("{:?} => {:?}", did, stub_instance.def_id());
      stub_instance.def_id()
    };

    // let an entry in stubs override our builtin stubs (see `self::stubs`):
    if self.stubs.len() != 0 {
      if let Some(&ki) = self.stubs.get(&path) {
        return convert_ki(ki);
      }
    }

    if !self.builtins { return did; }

    // check for the builtins:

    macro_rules! check {
      ($path:expr, $kid_did:expr, $abs_path:expr, $stub:expr) => {
        if $path == $abs_path {
          let ki = $stub.kernel_instance();
          return convert_ki(ki);
        } else {
          if $abs_path.starts_with("core::") && $path.starts_with("rustc_std_workspace_core::") {
            let suffix = &$abs_path["core::".len()..];
            let path_suffix = &$path["rustc_std_workspace_core::".len()..];
            if suffix == path_suffix {
              let ki = $stub.kernel_instance();
              return convert_ki(ki);
            }
          }

          if path.contains(stringify!($stub)) {
            warn!("missed stub: {}", path);
          }
        }
      };
    }
    macro_rules! check_alloc {
      (@detail $path:expr, $kid_did:expr, $flavor:ident, $stub:ident) => {
        {
          let check_path = concat!("alloc::", stringify!($flavor), "::",
                                   stringify!($stub));
          check!($path, $kid_did, check_path, $stub);
        }
      };

      ($path:expr, $kid_did:expr, $($stub:ident),*) => {$(
        check_alloc!(@detail $path, $kid_did, alloc, $stub);
      )*};
    }

    // Before check_alloc! and no warnings are emitted; but after and it fails
    // every time??
    check!(path, kid_did, "alloc::alloc::__rust_alloc_zeroed",
           __rust_alloc_zeroed);
    check_alloc!(path, kid_did,
                 __rust_alloc,
                 __rust_oom,
                 __rust_dealloc,
                 __rust_usable_size,
                 __rust_realloc,
                 __rust_alloc_excess,
                 __rust_realloc_excess,
                 __rust_grow_in_place,
                 handle_alloc_error);

    check!(path, kid_did, "std::panicking::rust_panic_with_hook",
           rust_panic_with_hook);
    // These two live in std::panicking, but their paths are in std::rt
    // for some reason.
    check!(path, kid_did, "std::rt::begin_panic",
           begin_panic::<()>);
    check!(path, kid_did, "std::rt::begin_panic_fmt",
           begin_panic_fmt);
    check!(path, kid_did, "core::panicking::panic_fmt::panic_impl",
           panic_impl);
    check!(path, kid_did, "core::panicking::panic_fmt",
           panic_fmt);
    check!(path, kid_did, "core::panicking::panic_bounds_check",
           panic_bounds_check);

    check!(path, kid_did, "core::slice::slice_index_order_fail",
           slice_index_order_fail);
    check!(path, kid_did, "core::slice::slice_index_len_fail",
           slice_index_len_fail);
    check!(path, kid_did, "core::slice::slice_index_overflow_fail",
           slice_index_overflow_fail);
    check!(path, kid_did, "core::str::slice_error_fail",
           slice_error_fail);

    check!(path, kid_did, "panic_unwind::real_imp::rust_eh_personality",
           rust_eh_personality);

    did
  }
}

impl Stubber {
  /// We need to force a few functions to be used. DO NOT CALL.
  #[doc(hidden)]
  pub fn force_mir<T>()
    where T: Default + ::std::any::Any + Send,
  {
    stubs::begin_panic(T::default(), &("", 0, 0));
  }
}

impl Default for Stubber {
  fn default() -> Self {
    Stubber {
      builtins: true,
      stubs: Default::default(),
    }
  }
}

mod stubs {
  #![allow(unused_variables)]

  use std::any::Any;
  use std::fmt;
  use core::alloc::Layout;
  use core::panic::{Location, PanicInfo, BoxMeUp, };

  unsafe fn abort() -> ! {
    use std::geobacter::intrinsics::geobacter_suicide;
    geobacter_suicide("stubbed function called");
  }

  // ALLOC
  pub fn __rust_alloc(size: usize, align: usize) -> *mut u8 {
    unsafe { abort() };
  }
  pub fn __rust_oom(err: *const u8) -> ! {
    unsafe { abort() };
  }
  pub fn __rust_dealloc(ptr: *mut u8, size: usize, align: usize) {
    unsafe { abort() };
  }
  pub fn __rust_usable_size(layout: *const u8,
                            min: *mut usize,
                            max: *mut usize) {
    unsafe { abort() };
  }
  pub fn __rust_realloc(ptr: *mut u8,
                        old_size: usize,
                        align: usize,
                        new_size: usize) -> *mut u8 {
    unsafe { abort() };
  }
  pub fn __rust_alloc_zeroed(size: usize, align: usize) -> *mut u8 {
    unsafe { abort() };
  }
  pub fn __rust_alloc_excess(size: usize,
                             align: usize,
                             excess: *mut usize,
                             err: *mut u8) -> *mut u8 {
    unsafe { abort() };
  }
  pub fn __rust_realloc_excess(ptr: *mut u8,
                               old_size: usize,
                               old_align: usize,
                               new_size: usize,
                               new_align: usize,
                               excess: *mut usize,
                               err: *mut u8) -> *mut u8 {
    unsafe { abort() };
  }
  pub fn __rust_grow_in_place(ptr: *mut u8,
                              old_size: usize,
                              old_align: usize,
                              new_size: usize,
                              new_align: usize) -> u8 {
    unsafe { abort() };
  }
  pub fn __rust_shrink_in_place(ptr: *mut u8,
                                old_size: usize,
                                old_align: usize,
                                new_size: usize,
                                new_align: usize) -> u8 {
    unsafe { abort() };
  }
  pub fn handle_alloc_error(_layout: Layout) -> ! {
    unsafe { abort() };
  }

  // PANIC
  #[inline(always)]
  pub fn begin_panic<M>(_msg: M, _file_line_col: &(&'static str, u32, u32)) -> !
    where M: Any + Send,
  {
    unsafe { abort() }
  }
  #[inline(always)]
  pub fn rust_panic_with_hook(_payload: &mut dyn BoxMeUp,
                              _message: Option<&fmt::Arguments>,
                              _file_line_col: &(&str, u32, u32))
    -> !
  {
    unsafe { abort() }
  }
  #[inline(always)]
  pub fn panic_impl(_: &PanicInfo) -> ! {
    unsafe { abort() }
  }
  #[inline(always)]
  pub fn panic_fmt(_fmt: fmt::Arguments,
                   _location: &Location<'_>)
    -> !
  {
    unsafe { abort() }
  }
  /// this one is in std
  #[inline(always)]
  pub fn begin_panic_fmt(_fmt: &fmt::Arguments,
                         _file_line_col: &(&'static str, u32, u32))
    -> !
  {
    unsafe { abort() }
  }
  #[inline(always)]
  pub fn panic_bounds_check(index: usize, len: usize, location: &Location<'_>) -> ! {
    // XXX this should probably be implemented.
    unsafe { abort() }
  }

  // Slice index panicking
  // XXX
  // This defeats the purpose of Rust... Except the type system <3
  // We need to override these so we don't try to `format!()`, which will
  // result in indirect function calls. AMDGPU doesn't support indirect
  // calls.
  // XXX
  #[inline(always)]
  pub fn slice_index_len_fail(_index: usize, _len: usize) -> ! {
    unsafe { abort() }
  }

  #[inline(always)]
  pub fn slice_index_order_fail(_index: usize, _end: usize) -> ! {
    unsafe { abort() }
  }

  #[inline(always)]
  pub fn slice_index_overflow_fail() -> ! {
    unsafe { abort() }
  }
  #[inline(always)]
  pub fn slice_error_fail(_s: &str, _begin: usize, _end: usize) -> ! {
    unsafe { abort() }
  }

  // Never called.
  pub fn rust_eh_personality() {}
}
