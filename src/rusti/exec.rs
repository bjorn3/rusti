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
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::str::from_utf8;
use std::sync::{Arc, Mutex};
use std::thread::Builder;

use rustc::dep_graph::DepGraph;
use rustc::hir::map as ast_map;
use rustc::ty;
use rustc::session::config::{self, basic_options, ErrorOutputType, Options, OptLevel};
use rustc_driver::driver;
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
}

impl ExecutionEngine {
    /// Constructs a new `ExecutionEngine` with the given library search paths.
    pub fn new(libs: Vec<String>, sysroot: Option<PathBuf>) -> ExecutionEngine {
        let sysroot = sysroot.unwrap_or_else(get_sysroot);

        let ee = ExecutionEngine{
            lib_paths: libs,
            sysroot: sysroot,
        };

        ee
    }

    pub fn call_function_with_source(&mut self, source: &str, name: &str) -> bool {
        let mut file = ::std::fs::OpenOptions::new().write(true).create(true).truncate(true).open("rusti_tmp_source.rs").unwrap();
        writeln!(file, "{}", source).unwrap();
        write!(file, "fn main() {{ {}(); }}", name).unwrap();
        let args = &[
            //"rustc".to_string(),
            "--sysroot".to_string(),
            self.sysroot.to_str().unwrap().to_owned(),
            "--crate-type".to_string(), "bin".to_string(),
            "rusti_tmp_source.rs".to_string(),
        ];
        println!("rustc args: {:?}", args);
        if !Command::new("rustc").args(args).status().unwrap().success() {
            return false;
        }
        Command::new("./rusti_tmp_source").status().unwrap();
        true
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
