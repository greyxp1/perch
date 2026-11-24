#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    clippy::all
)]
mod codec_ffi {
    include!(concat!(env!("OUT_DIR"), "/codecs.rs"));
}

/// Owns non-null C handles and releases them on drop.
macro_rules! handle {
    ($($vis:vis $name:ident($type:ty) = $free:path;)*) => {$(
        $vis struct $name($vis std::ptr::NonNull<$type>);
        impl Drop for $name {
            fn drop(&mut self) {
                unsafe { $free(self.0.as_ptr()) };
            }
        }
    )*};
}

mod app;
mod color;
mod decode;
mod document;
mod edit;
mod encode;
mod ipc;
mod overview;
mod shm;

use std::{ffi::OsString, os::fd::AsRawFd, process::ExitCode};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const RENDER_BYTES: usize = 256 * 1024 * 1024;

const HELP: &str = "\
Usage: perch IMAGE...
       perch --stdin
       perch --daemon
       perch convert IMAGE... --to FORMAT [OPTIONS]
       perch compress IMAGE... [OPTIONS]

Opens images in the running daemon, or converts and compresses them without it.
Run perch convert --help or perch compress --help for their options.
";

fn fd_path(file: &impl AsRawFd) -> String {
    format!("/proc/self/fd/{}", file.as_raw_fd())
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let result = match args.first().and_then(|arg| arg.to_str()) {
        Some("--daemon") if args.len() == 1 => app::run(),
        Some("-h" | "--help") if args.len() == 1 => {
            print!("{HELP}");
            Ok(())
        }
        Some(command @ ("convert" | "compress")) => encode::cli::run(command, &args[1..]),
        _ => ipc::run_client(&args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("perch: {error}");
            ExitCode::FAILURE
        }
    }
}
