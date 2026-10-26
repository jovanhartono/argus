use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

const USAGE: &str = "usage: tl index <folder>";

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [command, folder] if command == "index" => index(Path::new(folder)),
        [flag] if flag == "--version" => {
            println!("tl {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn index(folder: &Path) -> ExitCode {
    let started = Instant::now();
    let scan = match telescope_core::scan(folder) {
        Ok(scan) => scan,
        Err(error) => {
            eprintln!("tl: cannot scan {}: {error}", folder.display());
            return ExitCode::FAILURE;
        }
    };
    let seconds = started.elapsed().as_secs_f64();

    let index = &scan.index;
    let folders = (0..index.len() as u32).filter(|&e| index.is_dir(e)).count();
    println!(
        "{} entries ({folders} folders), {} distinct names",
        index.len(),
        index.distinct_names()
    );
    println!(
        "index memory: {:.1} MB",
        index.heap_bytes() as f64 / 1_048_576.0
    );
    println!("scan time: {seconds:.2} s");
    if scan.unreadable_folders > 0 {
        println!("folders not fully read: {}", scan.unreadable_folders);
    }
    ExitCode::SUCCESS
}
