#![feature(custom_attribute)]
#![feature(core_intrinsics)]
#![feature(slice_from_raw_parts)]

extern crate ndarray as nd;
extern crate ndarray_parallel as ndp;
extern crate env_logger;
extern crate rand;
extern crate packed_simd;

extern crate legionella_runtime_core as rt_core;
extern crate legionella_runtime_amd as rt_amd;
extern crate legionella_amdgpu_std as amdgpu_std;

use std::mem::{size_of, };
use std::time::Instant;

use ndp::prelude::*;

use packed_simd::{f64x8, };

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng, };

use rt_core::context::{Context, };
use rt_amd::HsaAmdGpuAccel;
use rt_amd::module::{Invoc, ArgsPool, };
use rt_amd::signal::*;

use amdgpu_std::{dispatch_packet, };

pub type Elem = f64;
pub type Simd = f64x8;
const COUNT_MUL: usize = 2;
const COUNT: usize = 1024 * 1024 * COUNT_MUL;
const ITERATIONS: usize = 16;

const WG_SIZE: usize = 8;

/// This is the kernel that is run on the GPU
pub fn vector_foreach(args: Args) {
  if let Some(tensor) = args.tensor_view() {
    let value = args.value;

    let idx = dispatch_packet().global_id_x();

    let dest = &mut tensor[idx];
    for _ in 0..ITERATIONS {
      *dest += 1.0;
      *dest *= value;
    }
  }
}

#[repr(C)] // Ensure we have a universally understood layout
#[derive(Clone, Copy)]
pub struct Args {
  tensor: *mut [Simd],
  pub value: Elem,
}

impl Args {
  pub fn tensor_view(&self) -> Option<&mut [Simd]> {
    unsafe {
      self.tensor.as_mut()
    }
  }
}

pub fn time<F, R>(what: &str, f: F) -> R
  where F: FnOnce() -> R,
{
  let start = Instant::now();
  let r = f();
  let elapsed = start.elapsed();
  println!("{} took {}ms ({}μs)", what,
           elapsed.as_millis(), elapsed.as_micros());

  r
}

pub fn main() {
  env_logger::init();
  let ctxt = Context::new()
    .expect("create context");

  let accels = HsaAmdGpuAccel::all_devices(&ctxt)
    .expect("HsaAmdGpuAccel::all_devices");
  if accels.len() < 1 {
    panic!("no accelerator devices???");
  }

  println!("allocating {} MB of host memory",
           COUNT * size_of::<Simd>() / 1024 / 1024);

  let mut original_values: Vec<Simd> = Vec::new();
  time("alloc original_values", || {
    original_values.reserve(COUNT)
  });
  unsafe {
    // no initialization:
    original_values.set_len(COUNT);
  }
  let mut rng = SmallRng::from_entropy();

  // run the kernel 20 times for good measure.
  const RUNS: usize = 20;

  for iteration in 0..RUNS {
    println!("Testing iteration {}/{}..", iteration, RUNS);

    for value in original_values.iter_mut() {
      *value = rng.gen();
    }

    for accel in accels.iter() {
      println!("Testing device {}", accel.agent().name().unwrap());

      let mut values = time("alloc host slice", || unsafe {
        accel.alloc_host_visible_slice(COUNT)
          .expect("HsaAmdGpuAccel::alloc_host_visible_slice")
      });
      time("copy original values", || {
        values.copy_from_slice(&original_values);
      });
      time("grant gpu access", || {
        values.set_accessible(&[&*accel])
          .expect("grant_agents_access");
      });

      let device_values_ptr = time("alloc device slice", || unsafe {
        accel.alloc_device_local_slice::<Simd>(COUNT)
          .expect("HsaAmdGpuAccel::alloc_device_local")
      });

      println!("host ptr: 0x{:p}, agent ptr: 0x{:p}", values, device_values_ptr);

      let async_copy_signal = accel.new_device_signal(1)
        .expect("HsaAmdGpuAccel::new_device_signal: async_copy_signal");
      let kernel_signal = accel.new_host_signal(1)
        .expect("HsaAmdGpuAccel::new_host_signal: kernel_signal");
      let results_signal = accel.new_host_signal(1)
        .expect("HsaAmdGpuAccel::new_host_signal: results_signal");

      unsafe {
        accel.unchecked_async_copy_into(values.as_pool_ptr().into_bytes(),
                                        device_values_ptr.as_pool_ptr().into_bytes(),
                                        &[], &async_copy_signal)
          .expect("HsaAmdGpuAccel::async_copy_into");
      }

      let mut invoc: Invoc<_, _, _, DeviceSignal> =
        Invoc::new(&accel, vector_foreach)
          .expect("Invoc::new");
      invoc.add_dep(async_copy_signal);
      unsafe {
        invoc.no_acquire_fence();
        invoc.device_release_fence();
      }

      let queue = accel.create_single_queue2(None, 0, 0)
        .expect("HsaAmdGpuAccel::create_single_queue");

      let args_pool = time("alloc kernargs pool", || {
        ArgsPool::new::<(Args, )>(&accel, 1)
          .expect("ArgsPool::new")
      });

      const VALUE: Elem = 4.0;
      let args = Args {
        tensor: device_values_ptr.as_ptr(),
        value: VALUE,
      };

      invoc.workgroup_dims((WG_SIZE, ));
      invoc.grid_dims((COUNT, ));

      println!("dispatching...");
      let wait = time("dispatching", || unsafe {
        invoc.unchecked_call_async((args, ), &queue,
                                   kernel_signal, &args_pool)
          .expect("Invoc::call_async")
      });

      // specifically wait (without enqueuing another async copy) here
      // so we can time just the dispatch.
      time("dispatch wait", move || {
        wait.wait(true);
      });

      // now copy the results back to the locked memory:
      unsafe {
        accel.unchecked_async_copy_from(device_values_ptr.as_pool_ptr().into_bytes(),
                                        values.as_pool_ptr().into_bytes(),
                                        &[], &results_signal)
          .expect("HsaAmdGpuAccel::async_copy_from");
      }
      time("gpu -> cpu async copy", || {
        results_signal.wait_for_zero(true)
          .expect("unexpected signal status");
      });

      let values = nd::aview1(&values);
      let original_values = nd::aview1(&original_values);

      // check results:
      time("checking results", || {
        nd::Zip::from(&values)
          .and(&original_values)
          .par_apply(|&lhs, &rhs| {
            let mut expected_value = rhs;
            for _ in 0..ITERATIONS {
              expected_value += 1.0;
              expected_value *= VALUE;
            }

            assert_eq!(lhs, expected_value);
          });
      });
    }
  }
}