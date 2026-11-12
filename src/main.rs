mod display;
mod image;
mod ipc;

use std::{ffi::OsString, process::ExitCode};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

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
