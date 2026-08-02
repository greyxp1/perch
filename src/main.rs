#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    clippy::all
)]
mod codecs {
    include!(concat!(env!("OUT_DIR"), "/codecs.rs"));
}

mod color;
mod display;
mod document;
mod extra;
mod image;
mod ipc;
mod jpeg;
mod loader;
mod native;
mod overview;

use std::{ffi::OsString, process::ExitCode};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const RENDER_BYTES: usize = 256 * 1024 * 1024;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let result = if args.len() == 1 && args[0] == "--daemon" {
        display::run()
    } else {
        ipc::run_client(&args)
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("perch: {error}");
            ExitCode::FAILURE
        }
    }
}
