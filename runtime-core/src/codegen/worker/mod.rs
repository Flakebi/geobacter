//! The main entry point to codegen. This worker thread is responsible for
//! initializing a Rust compiler driver and tricking it into just running
//! codegen on whatever kernel we need to compile.
//!
//! This thread also stores a cache of previously completed codegens. It
//! used to be stored in the Context, however now that `KernelDesc` is
//! parameterized by the PlatformCodegen trait, it makes more sense to
//! store it here.
//! TODO the cache is in memory only. It would be a good idea to be able
//! write it to disk to free some memory.
//!

use std::any::Any;
use std::collections::{BTreeMap, };
use std::error::{Error, };
use std::io::{self, };
use std::marker::{PhantomData, };
use std::mem;
use std::sync::mpsc::{channel, Sender, Receiver, RecvTimeoutError, };
use std::sync::{Arc, Weak, };
use std::time::Duration;

use rustc;
use rustc::hir::def_id::{CrateNum, DefId};
use rustc::middle::cstore::EncodedMetadata;
use rustc::middle::exported_symbols::{SymbolExportLevel, };
use rustc::mir::{CustomIntrinsicMirGen, };
use rustc::ty::query::Providers;
use crate::rustc::ty::{self, TyCtxt, subst::SubstsRef, };
use rustc::util::common::{time, };
use rustc::util::nodemap::DefIdMap;
use rustc_data_structures::fx::{FxHashMap};
use rustc_data_structures::sync::{self, Lrc, };
use rustc_metadata;
use rustc_metadata::cstore::CStore;
use rustc_incremental;
use rustc_target::abi::{LayoutDetails, };
use syntax::feature_gate;
use syntax_pos::symbol::{Symbol, InternedString, };

use crossbeam::sync::WaitGroup;

use lintrinsics::{DefIdFromKernelId, GetDefIdFromKernelId,
                  LegionellaCustomIntrinsicMirGen,
                  LegionellaMirGen, CNums, };

use tempfile::{Builder as TDBuilder, };

use crate::{AcceleratorTargetDesc, context::Context,
            context::WeakContext, };
use crate::utils::{HashMap, StableHash, } ;

use crate::passes::{Pass, };

use self::error::IntoErrorWithKernelId;
pub use self::driver_data::{DriverData, PlatformDriverData, };

mod collector;
pub mod error;
mod driver_data;
mod util;

use super::{PlatformCodegen, CodegenComms, PKernelDesc, };
use super::products::*;
use crate::codegen::{PlatformIntrinsicInsert, };

const CRATE_NAME: &'static str = "legionella-cross-codegen";

// TODO we need to create a talk to a "host codegen" so that we can ensure
// that adt's have the same layout in the shader/kernel as on the host.
// TODO XXX only one codegen query is allowed at a time.
// TODO codegen worker workers (ie codegen multiple functions concurrently)

/// Since we can't send MIR directly, or even serialized (we'd have to
/// serialize the whole crate too), the host codegenner will be responsible
/// for creating wrapping code to extract host kernel args.
/// Initially, no MIR will be created, eg by extracting a part of a function,
/// so this won't result in new host functions being codegenned (any function
/// we can reach would also be reachable in the original compilation).
pub enum HostCreateFuncMessage {
  /// A unmodified function in some crate
  ImplDefId(TyCtxtLessKernelId),

}

/// DefId is used here because they *should* be identical over every
/// codegen, due to the shared CStore.
/// Additionally, we require all these queries to block, so that we can send
/// references of things. Normally, we would have no assertion that accel tcx
/// outlives the refs sent. It is still unsafe here!
pub(crate) enum HostQueryMessage {
  TyLayout { ty: &'static ty::Ty<'static>,
    wait: WaitGroup,
    ret: Sender<Result<LayoutDetails, Box<dyn Error + Sync + Send>>>,
  },
}

pub(crate) enum Message<P>
  where P: PlatformCodegen,
{
  /// So we can know when to exit.
  AddAccel(Weak<P::Device>),
  StartHostQuery {
    rx: Receiver<HostQueryMessage>,
  },
  Codegen {
    desc: PKernelDesc<P>,

    //host_codegen: Sender<HostQueryMessage>,

    ret: Sender<Result<Arc<PCodegenResults<P>>, error::Error>>,
  },
}

struct DefIdFromKernelIdGetter<P>(PhantomData<P>)
  where P: PlatformCodegen;
impl<P> GetDefIdFromKernelId for DefIdFromKernelIdGetter<P>
  where P: PlatformCodegen,
{
  fn with_self<F, R>(tcx: TyCtxt, f: F) -> R
    where F: FnOnce(&dyn DefIdFromKernelId) -> R,
  {
    PlatformDriverData::<P>::with(tcx, move |_tcx, pd| {
      f(pd.dd() as &dyn DefIdFromKernelId)
    })
  }
}
impl<P> Default for DefIdFromKernelIdGetter<P>
  where P: PlatformCodegen,
{
  fn default() -> Self {
    DefIdFromKernelIdGetter(PhantomData)
  }
}
type IntrinsicsMap = FxHashMap<InternedString, Lrc<dyn CustomIntrinsicMirGen>>;
pub struct PlatformIntrinsicInserter<'a, P>(&'a mut IntrinsicsMap, PhantomData<P>);
impl<'a, P> PlatformIntrinsicInsert for PlatformIntrinsicInserter<'a, P>
  where P: PlatformCodegen,
{
  fn insert_name<T>(&mut self, name: &str, intrinsic: T)
    where T: LegionellaCustomIntrinsicMirGen
  {
    let k = Symbol::intern(name).as_interned_str();
    let marker: DefIdFromKernelIdGetter<P> = DefIdFromKernelIdGetter::default();
    let v = LegionellaMirGen::wrap(intrinsic, &marker);
    self.0.insert(k, v);
  }
}

enum MaybeWeakContext {
  Weak(WeakContext),
  Strong(Context),
}
impl MaybeWeakContext {
  fn upgrade(&mut self) -> Option<&Context> {
    let ctx = match self {
      MaybeWeakContext::Weak(ctx) => ctx.upgrade(),
      MaybeWeakContext::Strong(ctx) => {
        return Some(ctx);
      },
    };

    if let Some(ctx) = ctx {
      let mut this = MaybeWeakContext::Strong(ctx);
      mem::swap(&mut this, self);
      match self {
        MaybeWeakContext::Strong(ctx) => Some(ctx),
        _ => unreachable!(),
      }
    } else {
      None
    }
  }
  fn downgrade(&mut self) {
    let ctx = match self {
      MaybeWeakContext::Weak(_) => {
        return;
      },
      MaybeWeakContext::Strong(ctx) => ctx.downgrade_ref(),
    };

    let mut this = MaybeWeakContext::Weak(ctx);
    mem::swap(&mut this, self);
  }
}

pub struct WorkerTranslatorData<P>
  where P: PlatformCodegen,
{
  pub(self) context: MaybeWeakContext,
  pub(crate) platform: P,
  pub target_desc: Arc<AcceleratorTargetDesc>,
  pub accels: Vec<Weak<P::Device>>,
  pub sess: rustc::session::Session,
  pub passes: Vec<Box<dyn Pass<P>>>,
  /// Some intrinsics which don't need to be created on every codegen
  intrinsics: IntrinsicsMap,
  cache: HashMap<PKernelDesc<P>, Arc<PCodegenResults<P>>>,
}
impl<P> WorkerTranslatorData<P>
  where P: PlatformCodegen,
{
  pub fn new(ctx: &Context,
             accel_desc: Arc<AcceleratorTargetDesc>,
             platform: P)
    -> io::Result<CodegenComms<P>>
  {
    use std::thread::Builder;

    use crate::passes::lang_item::LangItemPass;
    use crate::passes::compiler_builtins::CompilerBuiltinsReplacerPass;

    use rustc::middle::dependency_format::Dependencies;

    let (tx, rx) = channel();
    let context = ctx.clone();

    let name = format!("codegen thread for {}",
                       accel_desc.target.llvm_target);

    let f = move || {
      info!("codegen thread for {} startup",
            accel_desc.target.llvm_target);

      let weak_context = context.downgrade_ref();

      // XXX this keeps the context alive anyway.
      context.syntax_globals().with(move || {
        with_rustc_session(move |mut sess| {
          sess.crate_types.set(sess.opts.crate_types.clone());
          sess.recursion_limit.set(512);
          sess.allocator_kind.set(None);

          let mut deps: Dependencies = Default::default();
          deps.insert(sess.crate_types.get()[0], vec![]);
          sess.dependency_formats.set(deps);

          sess.init_features(feature_gate::Features::new());

          // TODO hash the accelerator target desc
          let dis = self::util::compute_crate_disambiguator(&sess);
          sess.crate_disambiguator.set(dis);
          accel_desc.rustc_target_options(&mut sess.target.target);

          let mut data = WorkerTranslatorData {
            context: MaybeWeakContext::Weak(weak_context),
            platform,
            target_desc: accel_desc,
            accels: vec![],
            sess,
            passes: vec![
              Box::new(LangItemPass),
              Box::new(CompilerBuiltinsReplacerPass),
            ],
            intrinsics: Default::default(),
            cache: Default::default(),
          };

          let mut inserter = PlatformIntrinsicInserter(&mut data.intrinsics,
                                                       PhantomData::<P>);
          data.platform
            .insert_intrinsics(&data.target_desc,
                               &mut inserter);

          data.thread(&rx);
        })
      })
    };

    let _ = Builder::new()
      .name(name)
      .spawn(f)?;

    Ok(CodegenComms(tx))
  }

  fn thread(&mut self, rx: &Receiver<Message<P>>) {

    /// Our code here runs amok of Rust's borrow checker. Which is why
    /// this code has become pretty ugly. Sorry 'bout that.

    enum InternalMessage<D> {
      Timeout,
      AddAccel(Weak<D>),
    }

    let to = Duration::from_secs(10);

    let mut recv_msg = None;

    // Ensure we don't timeout and quit before we've received the
    // first message (which should be an add accel message)
    let mut first_msg = true;

    'outer: loop {
      let internal_msg = 'inner: loop {
        let context = match self.context.upgrade() {
          Some(ctxt) => ctxt.clone(),
          None => {
            // the context can't be resurrected.
            return;
          },
        };
        let krate = create_empty_hir_crate();
        let dep_graph = rustc::dep_graph::DepGraph::new_disabled();
        let mut forest = rustc::hir::map::Forest::new(krate, &dep_graph);
        let mut defs = rustc::hir::map::definitions::Definitions::default();
        let disambiguator = self.sess.crate_disambiguator
          .borrow()
          .clone();
        defs.create_root_def("jit-methods", disambiguator);
        let defs = defs;

        'msg: loop {
          let msg = recv_msg
            .take()
            .unwrap_or_else(|| {
              rx.recv_timeout(to)
            });
          let msg = match msg {
            Ok(msg) => msg,
            Err(RecvTimeoutError::Timeout) => {
              break 'inner InternalMessage::Timeout;
            },
            Err(RecvTimeoutError::Disconnected) => {
              return;
            },
          };

          first_msg = false;

          // Recreate this every loop. You'll have to transmute to get around
          // lifetime error false positives.
          let mut arena = rustc::ty::AllArenas::new();

          match msg {
            Message::AddAccel(accel) => {
              break 'inner InternalMessage::AddAccel(accel);
            },
            Message::StartHostQuery { rx, } => {
              // ignore any errors
              /*let _ = {
                self.host_queries(context.clone(),
                                  context.cstore(),
                                  unsafe {
                                    ::std::mem::transmute(&mut arena)
                                  },
                                  unsafe {
                                    ::std::mem::transmute(&mut forest)
                                  },
                                  unsafe {
                                    ::std::mem::transmute(&defs)
                                  },
                                  &dep_graph,
                                  rx)
              };*/
            },
            Message::Codegen {
              desc,
              //host_codegen,
              ret,
            } => {
              if let Some(results) = self.cache.get(&desc) {
                let _ = ret.send(Ok(results.clone()));
              }

              let (host_codegen, _) = channel();
              let result = {
                self.codegen_kernel(desc.clone(),
                                    context.clone(),
                                    unsafe {
                                      ::std::mem::transmute(&mut arena)
                                    },
                                    unsafe {
                                      ::std::mem::transmute(&mut forest)
                                    },
                                    unsafe {
                                      ::std::mem::transmute(&defs)
                                    },
                                    &dep_graph,
                                    host_codegen)
                  .map(Arc::new)
              };

              match result {
                Ok(ref result) => {
                  self.cache.insert(desc, result.clone());
                },
                Err(_) => { },
              }

              let _ = ret.send(result);
            },
          }

          continue 'msg;
        }
      };

      match internal_msg {
        InternalMessage::Timeout => { },
        InternalMessage::AddAccel(accel) => {
          self.accels.push(accel);
          continue 'outer;
        },
      }

      let live = self.context.upgrade().is_some();
      let live = live && self.accels.iter()
        .any(|a| a.upgrade().is_some());
      if !live && !first_msg { return; }

      self.context.downgrade();

      // else wait for the next message, at which point we will reinitialize.
      match rx.recv() {
        Err(_) => { return; },
        Ok(msg) => {
          recv_msg = Some(Ok(msg));
        },
      }
    }
  }

  fn codegen_kernel<'a>(&'a self,
                            desc: PKernelDesc<P>,
                            context: Context,
                            arenas: &mut ty::AllArenas,
                            forest: &mut rustc::hir::map::Forest,
                            defs: &rustc::hir::map::definitions::Definitions,
                            dep_graph: &rustc::dep_graph::DepGraph,
                            host_codegen: Sender<HostQueryMessage>)
    -> Result<PCodegenResults<P>, error::Error>
  {
    use rustc::hir::def_id::{DefIndex};
    use self::util::get_codegen_backend;

    let id = desc.instance.kernel_id;

    let crate_name = Symbol::intern(id.crate_name);
    assert_eq!(crate_name.as_str(), id.crate_name);

    let crate_num = self
      .lookup_crate_num(id)
      .ok_or_else(|| {
        error::Error::NoCrateMetadata(id)
      })?;

    info!("translating defid {:?}:{}, hash: 0x{:x}",
          crate_num, id.index, desc.instance.stable_hash());

    let def_id = DefId {
      krate: crate_num,
      index: DefIndex::from_usize(id.index as usize),
    };
    assert!(!def_id.is_local());

    let codegen = get_codegen_backend(&self.sess);

    // extern only providers:
    let mut local_providers = rustc::ty::query::Providers::default();
    self::util::default_provide(&mut local_providers);
    codegen.provide(&mut local_providers);
    Self::providers_local(&mut local_providers);

    let mut extern_providers = local_providers.clone();
    self::util::default_provide_extern(&mut extern_providers);
    codegen.provide_extern(&mut extern_providers);
    Self::provide_extern_overrides(&mut extern_providers);

    let disk_cache = rustc_incremental::load_query_result_cache(&self.sess);

    let (tx, rx) = channel();

    let tmpdir = TDBuilder::new()
      .prefix("legionella-runtime-codegen-")
      .tempdir()
      .with_kernel_id(id)?;

    let out = rustc::session::config::OutputFilenames {
      out_directory: tmpdir.path().into(),
      out_filestem: "codegen.elf".into(),
      single_output_file: None,
      extra: "".into(),
      outputs: output_types(),
    };

    let map_crate = rustc::hir::map::map_crate(&self.sess,
                                               context.cstore(),
                                               forest, defs);
    let resolutions = rustc::ty::Resolutions {
      trait_map: Default::default(),
      maybe_unused_trait_imports: Default::default(),
      maybe_unused_extern_crates: Default::default(),
      export_map: Default::default(),
      extern_prelude: Default::default(),
      glob_map: Default::default(),
    };

    let mut intrinsics = self.intrinsics.clone();
    {
      let mut inserter = PlatformIntrinsicInserter(&mut intrinsics,
                                                   PhantomData::<P>);
      self.platform
        .insert_kernel_intrinsics(&desc, &mut inserter);
    }

    let driver_data: PlatformDriverData<P> =
      PlatformDriverData::new(context.clone(),
                              &self.accels,
                              Some(host_codegen),
                              &self.target_desc,
                              &self.passes,
                              intrinsics,
                              &self.platform);
    let driver_data: PlatformDriverData<'static, P> = unsafe {
      ::std::mem::transmute(driver_data)
    };
    let driver_data = Box::new(driver_data) as Box<dyn Any + Send + Sync>;

    let gcx = TyCtxt::create_global_ctxt(
      &self.sess,
      context.cstore(),
      local_providers,
      extern_providers,
      &arenas,
      resolutions,
      map_crate,
      disk_cache,
      CRATE_NAME,
      tx,
      &out,
      Some(driver_data),
    );

    let results: Result<PCodegenResults<P>, error::Error> = ty::tls::enter_global(&gcx, |tcx| {
      // Do some initialization of the DepGraph that can only be done with the
      // tcx available.
      time(tcx.sess, "dep graph tcx init", || rustc_incremental::dep_graph_tcx_init(tcx));

      time(tcx.sess, "platform root and condition init",
           move || {
             PlatformDriverData::<P>::with(tcx, |tcx, pd| {
               pd.init_root(desc, tcx)
                 .map_err(error::Error::InitRoot)?;

               pd.init_conditions(tcx)
                 .map_err(error::Error::InitConditions)
             })
           })?;

      let metadata = EncodedMetadata::new();
      let need_metadata_module = false;

      tcx.sess.profiler(|p| p.start_activity("codegen crate"));
      let ongoing_codegen = time(tcx.sess, "codegen", || {
        codegen.codegen_crate(tcx, metadata, need_metadata_module,
                              rx)
      });
      tcx.sess.profiler(|p| p.end_activity("codegen crate"));

      time(tcx.sess, "LLVM codegen",
           || {
             codegen.join_codegen_and_link(ongoing_codegen,
                                           &self.sess, dep_graph,
                                           &out)
               .map_err(|err| {
                 error!("codegen failed: `{:?}`!", err);
                 error::Error::Codegen(id)
               })
           })?;

      let results = PlatformDriverData::<P>::with(tcx, |tcx, pd| {
        pd.post_codegen(tcx, &tmpdir.path(), &out)
          .map_err(error::Error::PostCodegen)
      })?;

      Ok(results)
    });

    let mut results = results?;

    let output_dir = tmpdir.into_path();

    self.platform
      .post_codegen(&self.target_desc,
                    &output_dir,
                    &mut results)
      .map_err(error::Error::PostCodegen)?;
    // check that the platform actually inserted an exe entry:
    debug_assert!(results.exe_ref().is_some(),
      "internal platform codegen error: platform didn't insert an Exe \
       output type into the results");

    info!("codegen complete {:?}:{}", crate_num, id.index);

    info!("codegen intermediates dir: {}", output_dir.display());

    Ok(results)
  }
}
impl<P> DefIdFromKernelId for WorkerTranslatorData<P>
  where P: PlatformCodegen,
{
  fn get_cstore(&self) -> &CStore {
    match self.context {
      MaybeWeakContext::Weak(_) => {
        panic!("context is weak; no cstore available")
      },
      MaybeWeakContext::Strong(ref ctx) => ctx.cstore(),
    }
  }
  fn cnum_map(&self) -> Option<&CNums> {
    match self.context {
      MaybeWeakContext::Weak(_) => { None },
      MaybeWeakContext::Strong(ref ctx) => Some(ctx.cnums()),
    }
  }
}

fn output_types() -> rustc::session::config::OutputTypes {
  use rustc::session::config::*;

  let output = (OutputType::Bitcode, None);
  let ir_out = (OutputType::LlvmAssembly, None);
  //let asm = (OutputType::Assembly, None);
  //let exe = (OutputType::Exe, None);
  let mut out = Vec::new();
  out.push(output);
  out.push(ir_out);
  //out.push(asm);
  //out.push(exe);
  OutputTypes::new(&out[..])
}

pub fn with_rustc_session<F, R>(f: F) -> R
  where F: FnOnce(rustc::session::Session) -> R + sync::Send,
        R: sync::Send,
{
  use self::util::spawn_thread_pool;
  use crate::rustc_interface::util::diagnostics_registry;

  let opts = create_rustc_options();
  spawn_thread_pool(Some(1), move || {
    let registry = diagnostics_registry();
    let sess = rustc::session::build_session(opts, None, registry);
    f(sess)
  })
}
pub fn create_rustc_options() -> rustc::session::config::Options {
  use rustc::session::config::*;
  use rustc_target::spec::*;

  let mut opts = Options::default();
  opts.crate_types.push(CrateType::Cdylib);
  opts.output_types = output_types();
  opts.optimize = OptLevel::No;
  opts.optimize = OptLevel::Aggressive;
  opts.cg.lto = LtoCli::No;
  opts.cg.panic = Some(PanicStrategy::Abort);
  opts.cg.incremental = None;
  opts.cg.overflow_checks = Some(false);
  opts.cli_forced_codegen_units = Some(1);
  opts.incremental = None;
  opts.debugging_opts.verify_llvm_ir = false;
  opts.debugging_opts.no_landing_pads = true;
  opts.debugging_opts.incremental_queries = false;
  opts.debugging_opts.share_generics = Some(true);
  opts.cg.no_prepopulate_passes = false;
  if opts.cg.no_prepopulate_passes {
    opts.cg.passes.push("name-anon-globals".into());
  } else {
    // Should we run this unconditionally?
    opts.cg.passes.push("wholeprogramdevirt".into());
    opts.cg.passes.push("speculative-execution".into());
  }
  opts.debugging_opts.print_llvm_passes = false;
  opts.cg.llvm_args.push("-expensive-combines".into());
  opts.cg.llvm_args.push("-spec-exec-only-if-divergent-target".into());
  opts.debugging_opts.polly =
    opts.optimize == OptLevel::Aggressive;
  opts.cg.llvm_args.push("-polly-run-inliner".into());
  opts.cg.llvm_args.push("-polly-register-tiling".into());
  opts.cg.llvm_args.push("-polly-check-vectorizable".into());
  opts.cg.llvm_args.push("-enable-polly-aligned".into());
  // TODO: -polly-target=gpu produces host side code which
  // then triggers the gpu side code.
  //opts.cg.llvm_args.push("-polly-target=gpu".into());
  opts.cg.llvm_args.push("-polly-vectorizer=polly".into());
  opts.cg.llvm_args.push("-polly-position=early".into());
  opts.cg.llvm_args.push("-polly-enable-polyhedralinfo".into());
  opts
}

pub fn create_empty_hir_crate() -> rustc::hir::Crate {
  use rustc::hir::*;
  use syntax_pos::DUMMY_SP;

  let m = Mod {
    inner: DUMMY_SP,
    item_ids: HirVec::from(vec![]),
  };

  let attrs = HirVec::from(vec![]);
  let span = DUMMY_SP;
  let exported_macros = HirVec::from(vec![]);
  let items = BTreeMap::new();
  let trait_items = BTreeMap::new();
  let impl_items = BTreeMap::new();
  let bodies = BTreeMap::new();
  let trait_impls = BTreeMap::new();
  let modules = BTreeMap::new();

  let body_ids = Vec::new();

  Crate {
    module: m,
    attrs,
    span,
    exported_macros,
    items,
    trait_items,
    impl_items,
    bodies,
    trait_impls,
    body_ids,
    modules,
    non_exported_macro_attrs: Default::default(),
  }
}

impl<P> WorkerTranslatorData<P>
  where P: PlatformCodegen,
{
  fn replace(tcx: TyCtxt, def_id: DefId) -> DefId {
    PlatformDriverData::<P>::with(tcx, |tcx, pd| {
      pd.dd().replace_def_id(tcx, def_id)
    })
  }

  pub fn provide_mir_overrides(providers: &mut Providers) {
    *providers = Providers {
      is_mir_available: |tcx, def_id| {
        let mut providers = Providers::default();
        rustc_metadata::cstore::provide_extern(&mut providers);

        let stubber = lintrinsics::stubbing::Stubber::default();
        let def_id = PlatformDriverData::<P>::with(tcx, |tcx, pd| {
          stubber.stub_def_id(tcx, pd.dd(), def_id)
        });

        (providers.is_mir_available)(tcx, def_id)
      },
      optimized_mir: |tcx, def_id| {
        let mut providers = Providers::default();
        rustc_metadata::cstore::provide_extern(&mut providers);

        let stubber = lintrinsics::stubbing::Stubber::default();
        let def_id = PlatformDriverData::<P>::with(tcx, |tcx, pd| {
          stubber.stub_def_id(tcx, pd.dd(), def_id)
        });

        (providers.optimized_mir)(tcx, def_id)
      },
      symbol_name: |tcx, instance| {
        let mut providers = Providers::default();
        rustc_codegen_utils::symbol_names::provide(&mut providers);

        let stubber = lintrinsics::stubbing::Stubber::default();
        let instance = PlatformDriverData::<P>::with(tcx, |tcx, pd| {
          stubber.map_instance(tcx, pd.dd(), instance)
        });

        (providers.symbol_name)(tcx, instance)
      },
      ..*providers
    }
  }
  pub fn provide_extern_overrides(providers: &mut Providers) {
    Self::provide_mir_overrides(providers);
    Self::providers_remote_and_local(providers);
  }

  pub fn providers_local(providers: &mut Providers) {
    use rustc::hir::def_id::{LOCAL_CRATE};
    use rustc_data_structures::svh::Svh;

    *providers = Providers {
      crate_hash: |_tcx, cnum| {
        assert_eq!(cnum, LOCAL_CRATE);
        // XXX?
        Svh::new(1)
      },
      crate_name: |_tcx, id| {
        assert_eq!(id, LOCAL_CRATE);
        Symbol::intern(CRATE_NAME)
      },
      crate_disambiguator: |tcx, cnum| {
        assert_eq!(cnum, LOCAL_CRATE);
        tcx.sess.crate_disambiguator.borrow().clone()
      },
      native_libraries: |_tcx, cnum| {
        assert_eq!(cnum, LOCAL_CRATE);
        Lrc::new(vec![])
      },
      link_args: |_tcx, cnum| {
        assert_eq!(cnum, LOCAL_CRATE);
        Lrc::new(vec![])
      },
      type_of: |tcx, def_id| {
        PlatformDriverData::<P>::with(tcx, |_tcx, pd| {
          pd.dd().expect_type_of(def_id)
        })
      },
      ..*providers
    };
    Self::provide_mir_overrides(providers);
    Self::providers_remote_and_local(providers);
  }

  fn providers_remote_and_local(providers: &mut Providers) {
    use rustc::session::config::EntryFnType;

    *providers = Providers {
      fn_sig: |tcx, def_id| {
        use rustc::ty::{Binder, FnSig, };

        let mut providers = Providers::default();
        rustc_metadata::cstore::provide_extern(&mut providers);

        // no stubbing here. We want the original fn sig
        let def_id = Self::replace(tcx, def_id);

        let sig = (providers.fn_sig)(tcx, def_id);

        PlatformDriverData::<P>::with(tcx, |_tcx, pd| {
          if pd.dd().is_root(def_id) {
            // modify the abi:
            let sig = FnSig {
              abi: pd.dd().target_desc.kernel_abi.clone(),
              ..*sig.skip_binder()
            };
            Binder::bind(sig)
          } else {
            sig
          }
        })
      },
      reachable_non_generics,
      custom_intrinsic_mirgen: |tcx, def_id| {
        PlatformDriverData::<P>::with(tcx, |tcx, pd| {
          let name = tcx.item_name(def_id);
          debug!("custom intrinsic: {}", name);
          pd.dd()
            .intrinsics
            .read()
            .get(&name.as_interned_str())
            .cloned()
        })
      },
      upstream_monomorphizations,
      upstream_monomorphizations_for,
      codegen_fn_attrs: |tcx, def_id| {
        let mut providers = Providers::default();
        rustc_typeck::provide(&mut providers);

        let id = Self::replace(tcx, def_id);

        let mut attrs = (providers.codegen_fn_attrs)(tcx, id);

        PlatformDriverData::<P>::with(tcx, move |tcx, pd| {
          pd.platform.codegen_fn_attrs(tcx, &pd.driver_data,
                                       id, &mut attrs);


          if let Some(ref spirv) = attrs.spirv {
            info!("spirv attrs for {:?}: {:#?}", id, spirv);
          }

          attrs
        })
      },
      item_attrs: |tcx, def_id| {
        let mut providers = Providers::default();
        rustc_metadata::cstore::provide_extern(&mut providers);

        // Note: no replace here. For one, we'll introduce a cycle. And
        // we don't want to use the attributes of a different item anyway.

        let attrs = (providers.item_attrs)(tcx, def_id);

        PlatformDriverData::<P>::with(tcx, move |tcx, pd| {
          pd.platform.item_attrs(tcx, &pd.driver_data, attrs)
        })
      },
      entry_fn,
      collect_and_partition_mono_items: |tcx, cnum| {
        PlatformDriverData::<P>::with(tcx, move |tcx, pd| {
          collector::collect_and_partition_mono_items(tcx,
                                                      pd.dd(),
                                                      cnum)
        })
      },
      ..*providers
    };

    fn entry_fn<'tcx>(_tcx: TyCtxt<'tcx>, _cnum: CrateNum) -> Option<(DefId, EntryFnType)> {
      None
    }
    fn reachable_non_generics<'tcx>(tcx: TyCtxt<'tcx>, _cnum: CrateNum)
      -> &'tcx DefIdMap<SymbolExportLevel>
    {
      // we need to recodegen everything
      tcx.arena.alloc(Default::default())
    }
    fn upstream_monomorphizations(tcx: TyCtxt, _cnum: CrateNum)
      -> &DefIdMap<FxHashMap<SubstsRef, CrateNum>>
    {
      // we never have any upstream monomorphizations.
      tcx.arena.alloc(Default::default())
    }
    fn upstream_monomorphizations_for(_tcx: TyCtxt, _def_id: DefId)
      -> Option<&FxHashMap<SubstsRef, CrateNum>>
    {
      None
    }
  }
}

pub struct TyCtxtLessKernelId {
  pub crate_name: String,
  pub crate_hash_hi: u64,
  pub crate_hash_lo: u64,
  pub index: u64,
}
impl TyCtxtLessKernelId {
  pub fn from_def_id(tcx: TyCtxt<'_>,
                     def_id: DefId) -> Self {
    let crate_name = tcx.crate_name(def_id.krate);
    let crate_name = format!("{}", crate_name);

    let disambiguator = tcx.crate_disambiguator(def_id.krate);
    let (crate_hash_hi, crate_hash_lo) = disambiguator.to_fingerprint().as_value();

    let index = def_id.index.as_usize() as u64;

    TyCtxtLessKernelId {
      crate_name,
      crate_hash_hi,
      crate_hash_lo,
      index,
    }
  }
}
