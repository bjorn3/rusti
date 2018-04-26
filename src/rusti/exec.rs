// Copyright 2014-2016 Rusti Project
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Rust code parsing and compilation.

use std::any::Any;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::str::from_utf8;
use std::sync::{Arc, Mutex};
use std::thread::Builder;
use std::cell::RefCell;

use getopts::Matches;

use rustc::dep_graph::DepGraph;
use rustc::hir::map as ast_map;
use rustc::ty;
use rustc::session::Session;
use rustc::session::config::{self, basic_options, ErrorOutputType, Options, OptLevel};
use rustc_driver::Compilation;
use rustc_driver::driver::CompileController;
use rustc_metadata::cstore::CStore;

use syntax::codemap::{FileName, MultiSpan};
use syntax::errors;
use syntax::errors::emitter::EmitterWriter;
use syntax::feature_gate::UnstableFeatures;

/// Compiles input code into an execution environment.
pub struct ExecutionEngine {
    /// Additional search paths for libraries
    lib_paths: Vec<String>,
    sysroot: PathBuf,
    counter: u64,
}

impl ExecutionEngine {
    /// Constructs a new `ExecutionEngine` with the given library search paths.
    pub fn new(libs: Vec<String>, sysroot: Option<PathBuf>) -> ExecutionEngine {
        let sysroot = sysroot.unwrap_or_else(get_sysroot);

        let ee = ExecutionEngine{
            lib_paths: libs,
            sysroot: sysroot,
            counter: 0,
        };

        ee
    }

    pub fn call_function_with_source(&mut self, source: &str, name: &str) -> bool {
        let dylib_file = format!("./rusti_tmp_source_{}.dylib", self.counter);
        let _ = ::std::fs::remove_file(&dylib_file);
        let mut file = ::std::fs::OpenOptions::new().write(true).create(true).truncate(true).open("rusti_tmp_source.rs").unwrap();
        writeln!(file, "{}", source).unwrap();
        //write!(file, "fn main() {{ {}(); }}", name).unwrap();
        let args = &[
            //"rustc".to_string(),
            "-L".to_string(), ".".to_string(),
            "--sysroot".to_string(),
            self.sysroot.to_str().unwrap().to_owned(),
            "--crate-type".to_string(), "dylib".to_string(),
            "rusti_tmp_source.rs".to_string(),
            "-o".to_string(), dylib_file.clone(),
        ];
        debug!("rustc args: {:?} fn_name: {}", args, name);
        if !Command::new("rustc").args(args).status().unwrap().success() {
            return false;
        }
        //Command::new("./rusti_tmp_source").status().unwrap();
        unsafe {
            let lib = ::libloading::Library::new(&dylib_file).unwrap();
            {
                let func: ::libloading::Symbol<unsafe extern fn() -> ()> = lib.get(name.as_bytes()).unwrap();
                func();
            }
            // Don't unload lib, to prevent segv when for example a thread is still running.
            ::std::mem::forget(lib);
        }
        self.counter += 1;
        true
    }

    pub fn with_tcx<T>(&self, prog: String, f: Box<Fn(ty::TyCtxt) -> T>) -> T {
        struct MyFileLoader(String);
        impl ::syntax::codemap::FileLoader for MyFileLoader {
            fn file_exists(&self, _path: &Path) -> bool {
                true
            }
            fn abs_path(&self, _path: &Path) -> Option<PathBuf> {
                None
            }
            fn read_file(&self, _path: &Path) -> ::std::io::Result<String> {
                Ok(self.0.clone())
            }
        }

        struct MyCb<T>(Rc<Fn(ty::TyCtxt) -> T>, Rc<RefCell<Option<T>>>);
        impl<'a, T: 'a> ::rustc_driver::CompilerCalls<'a> for MyCb<T> {
            fn build_controller(&mut self, _: &Session, _: &Matches) -> CompileController<'a> {
                let f = self.0.clone();
                let res = self.1.clone();
                let mut controller = CompileController::basic();
                controller.after_analysis.stop = Compilation::Stop;
                controller.after_analysis.callback = Box::new(move |state| {
                    *res.borrow_mut() = Some(f(state.tcx.unwrap()));
                });
                controller
            }
        }

        let mut cb = MyCb(f.into(), Rc::new(RefCell::new(None)));
        let loader = MyFileLoader(prog);
        ::rustc_driver::run_compiler(&[
            "rustc".to_string(),
            "dummy_name".to_string(),
            "--crate-type".to_string(), "lib".to_string(),
            "--sysroot".to_string(),
            self.sysroot.to_str().unwrap().to_owned(),
        ], &mut cb, Some(Box::new(loader)), None);
        let ret = cb.1.borrow_mut().take().unwrap();
        ret
    }
}

struct SyncBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SyncBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

/// Runs `rustc` to ask for its sysroot path.
fn get_sysroot() -> PathBuf {
    let rustc = if cfg!(windows) { "rustc.exe" } else { "rustc" };

    let output = match Command::new(rustc).args(&["--print", "sysroot"]).output() {
        Ok(output) => output.stdout,
        Err(e) => panic!("failed to run rustc: {}", e),
    };

    let path = from_utf8(&output)
        .ok().expect("sysroot is not valid UTF-8").trim_right_matches(
            |c| c == '\r' || c == '\n');

    debug!("using sysroot: {:?}", path);

    PathBuf::from(path)
}

fn build_exec_options(sysroot: PathBuf, libs: Vec<String>) -> Options {
    let mut opts = basic_options();

    // librustc derives sysroot from the executable name.
    // Since we are not rustc, we must specify it.
    opts.maybe_sysroot = Some(sysroot);

    for p in libs.iter() {
        opts.search_paths.add_path(&p,
            ErrorOutputType::HumanReadable(errors::ColorConfig::Auto));
    }

    // Prefer faster build times
    opts.optimize = OptLevel::No;

    // Don't require a `main` function
    opts.crate_types = vec![config::CrateTypeDylib];

    // Allow use of unstable features
    opts.unstable_features = UnstableFeatures::Allow;

    opts
}

fn monitor<F, R>(f: F) -> Option<R>
        where F: Send + 'static + FnOnce() -> R, R: Send + 'static {
    let thread = Builder::new().name("compile_input".to_owned());
    let data = Arc::new(Mutex::new(Vec::new()));
    let sink = SyncBuf(data.clone());

    let handle = thread.spawn(move || {
        if !log_enabled!(::log::Level::Debug) {
            io::set_panic(Some(Box::new(sink)));
        }
        f()
    }).unwrap();

    match handle.join() {
        Ok(r) => Some(r),
        Err(e) => {
            handle_compiler_panic(e, data);
            None
        }
    }
}

fn handle_compiler_panic(e: Box<Any + Send + 'static>, data: Arc<Mutex<Vec<u8>>>) {
    if !e.is::<errors::FatalError>() {
        if !e.is::<errors::ExplicitBug>() {
            let emitter = EmitterWriter::stderr(errors::ColorConfig::Auto,
                None, false, false);

            let handler = errors::Handler::with_emitter(
                true, false, Box::new(emitter));

            handler.emit(
                &MultiSpan::new(),
                "unexpected panic",
                errors::Level::Bug);
        }

        print!("{}", from_utf8(&data.lock().unwrap()).unwrap());
    }
}
