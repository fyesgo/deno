// Copyright 2018-2019 the Deno authors. All rights reserved. MIT license.
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate log;
#[macro_use]
extern crate futures;
#[macro_use]
extern crate serde_json;
extern crate clap;
extern crate deno;
#[cfg(unix)]
extern crate nix;
extern crate rand;

mod ansi;
pub mod compiler;
pub mod deno_dir;
pub mod diagnostics;
mod dispatch_minimal;
pub mod errors;
pub mod flags;
mod fs;
mod global_timer;
mod http_body;
mod http_util;
pub mod js_errors;
pub mod msg;
pub mod msg_util;
pub mod ops;
pub mod permissions;
mod progress;
mod repl;
pub mod resolve_addr;
pub mod resources;
mod signal;
mod startup_data;
pub mod state;
mod tokio_util;
mod tokio_write;
pub mod version;
pub mod worker;

use crate::errors::RustOrJsError;
use crate::progress::Progress;
use crate::state::ThreadSafeState;
use crate::worker::root_specifier_to_url;
use crate::worker::Worker;
use deno::v8_set_flags;
use flags::DenoFlags;
use flags::DenoSubcommand;
use futures::lazy;
use futures::Future;
use log::{LevelFilter, Metadata, Record};
use std::env;

static LOGGER: Logger = Logger;

struct Logger;

impl log::Log for Logger {
  fn enabled(&self, metadata: &Metadata) -> bool {
    metadata.level() <= log::max_level()
  }

  fn log(&self, record: &Record) {
    if self.enabled(record.metadata()) {
      let mut target = record.target().to_string();

      if let Some(line_no) = record.line() {
        target.push_str(":");
        target.push_str(&line_no.to_string());
      }

      println!("{} RS - {} - {}", record.level(), target, record.args());
    }
  }
  fn flush(&self) {}
}

fn print_err_and_exit(err: RustOrJsError) {
  eprintln!("{}", err.to_string());
  std::process::exit(1);
}

fn js_check<E>(r: Result<(), E>)
where
  E: Into<RustOrJsError>,
{
  if let Err(err) = r {
    print_err_and_exit(err.into());
  }
}

// TODO(ry) Move this to main.rs
pub fn print_file_info(worker: &Worker, url: &str) {
  let maybe_out =
    worker::fetch_module_meta_data_and_maybe_compile(&worker.state, url, ".");
  if let Err(err) = maybe_out {
    println!("{}", err);
    return;
  }
  let out = maybe_out.unwrap();

  println!("{} {}", ansi::bold("local:".to_string()), &(out.filename));

  println!(
    "{} {}",
    ansi::bold("type:".to_string()),
    msg::enum_name_media_type(out.media_type)
  );

  if out.maybe_output_code_filename.is_some() {
    println!(
      "{} {}",
      ansi::bold("compiled:".to_string()),
      out.maybe_output_code_filename.as_ref().unwrap(),
    );
  }

  if out.maybe_source_map_filename.is_some() {
    println!(
      "{} {}",
      ansi::bold("map:".to_string()),
      out.maybe_source_map_filename.as_ref().unwrap()
    );
  }

  if let Some(deps) = worker.modules.deps(&out.module_name) {
    println!("{}{}", ansi::bold("deps:\n".to_string()), deps.name);
    if let Some(ref depsdeps) = deps.deps {
      for d in depsdeps {
        println!("{}", d);
      }
    }
  } else {
    println!(
      "{} cannot retrieve full dependency graph",
      ansi::bold("deps:".to_string()),
    );
  }
}

fn create_worker_and_state(
  flags: DenoFlags,
  argv: Vec<String>,
) -> (Worker, ThreadSafeState) {
  let progress = Progress::new();
  progress.set_callback(|done, completed, total, msg| {
    if !done {
      eprint!("\r[{}/{}] {}", completed, total, msg);
      eprint!("\x1B[K"); // Clear to end of line.
      return;
    }

    // print empty line only if progress bar was used
    if done && total > 0 {
      eprintln!();
    }
  });
  let state = ThreadSafeState::new(flags, argv, ops::op_selector_std, progress);
  let worker = Worker::new(
    "main".to_string(),
    startup_data::deno_isolate_init(),
    state.clone(),
  );

  (worker, state)
}

fn types_command() {
  let content = include_str!(concat!(
    env!("GN_OUT_DIR"),
    "/gen/cli/lib/lib.deno_runtime.d.ts"
  ));
  println!("{}", content);
}

fn fetch_or_info_command(
  flags: DenoFlags,
  argv: Vec<String>,
  print_info: bool,
) {
  let (mut worker, state) = create_worker_and_state(flags, argv);

  let main_module = state.main_module().unwrap();
  let main_future = lazy(move || {
    // Setup runtime.
    js_check(worker.execute("denoMain()"));
    debug!("main_module {}", main_module);

    let main_url = root_specifier_to_url(&main_module).unwrap();

    worker
      .execute_mod_async(&main_url, true)
      .and_then(move |worker| {
        if print_info {
          print_file_info(&worker, &main_module);
        }
        worker.then(|result| {
          js_check(result);
          Ok(())
        })
      }).map_err(|(err, _worker)| print_err_and_exit(err))
  });
  tokio_util::run(main_future);
}

fn eval_command(flags: DenoFlags, argv: Vec<String>) {
  let (mut worker, state) = create_worker_and_state(flags, argv);
  // Wrap provided script in async function so asynchronous methods
  // work. This is required until top-level await is not supported.
  let js_source = format!(
    "async function _topLevelWrapper(){{
        {}
      }}
      _topLevelWrapper();
      ",
    &state.argv[1]
  );

  let main_future = lazy(move || {
    js_check(worker.execute("denoMain()"));
    // ATM imports in `deno eval` are not allowed
    // TODO Support ES modules once Worker supports evaluating anonymous modules.
    js_check(worker.execute(&js_source));
    worker.then(|result| {
      js_check(result);
      Ok(())
    })
  });
  tokio_util::run(main_future);
}

fn xeval_command(flags: DenoFlags, argv: Vec<String>) {
  let xeval_replvar = flags.xeval_replvar.clone().unwrap();
  let (mut worker, state) = create_worker_and_state(flags, argv);
  let xeval_source = format!(
    "window._xevalWrapper = async function ({}){{
        {}
      }}",
    &xeval_replvar, &state.argv[1]
  );

  let main_future = lazy(move || {
    // Setup runtime.
    js_check(worker.execute(&xeval_source));
    js_check(worker.execute("denoMain()"));
    worker
      .then(|result| {
        js_check(result);
        Ok(())
      }).map_err(|(err, _worker): (RustOrJsError, Worker)| {
        print_err_and_exit(err)
      })
  });
  tokio_util::run(main_future);
}

fn run_repl(flags: DenoFlags, argv: Vec<String>) {
  let (mut worker, _state) = create_worker_and_state(flags, argv);

  // REPL situation.
  let main_future = lazy(move || {
    // Setup runtime.
    js_check(worker.execute("denoMain()"));
    worker
      .then(|result| {
        js_check(result);
        Ok(())
      }).map_err(|(err, _worker): (RustOrJsError, Worker)| {
        print_err_and_exit(err)
      })
  });
  tokio_util::run(main_future);
}

fn run_script(flags: DenoFlags, argv: Vec<String>) {
  let (mut worker, state) = create_worker_and_state(flags, argv);

  let main_module = state.main_module().unwrap();
  // Normal situation of executing a module.
  let main_future = lazy(move || {
    // Setup runtime.
    js_check(worker.execute("denoMain()"));
    debug!("main_module {}", main_module);

    let main_url = root_specifier_to_url(&main_module).unwrap();

    worker
      .execute_mod_async(&main_url, false)
      .and_then(move |worker| {
        worker.then(|result| {
          js_check(result);
          Ok(())
        })
      }).map_err(|(err, _worker)| print_err_and_exit(err))
  });
  tokio_util::run(main_future);
}

fn main() {
  #[cfg(windows)]
  ansi_term::enable_ansi_support().ok(); // For Windows 10

  log::set_logger(&LOGGER).unwrap();
  let args: Vec<String> = env::args().collect();
  let (flags, subcommand, argv) = flags::flags_from_vec(args);

  if let Some(ref v8_flags) = flags.v8_flags {
    v8_set_flags(v8_flags.clone());
  }

  log::set_max_level(if flags.log_debug {
    LevelFilter::Debug
  } else {
    LevelFilter::Warn
  });

  match subcommand {
    DenoSubcommand::Eval => eval_command(flags, argv),
    DenoSubcommand::Fetch => fetch_or_info_command(flags, argv, false),
    DenoSubcommand::Info => fetch_or_info_command(flags, argv, true),
    DenoSubcommand::Repl => run_repl(flags, argv),
    DenoSubcommand::Run => run_script(flags, argv),
    DenoSubcommand::Types => types_command(),
    DenoSubcommand::Version => run_repl(flags, argv),
    DenoSubcommand::Xeval => xeval_command(flags, argv),
  }
}
