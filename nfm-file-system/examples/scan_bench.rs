#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    nfm_file_system::walker::benchmark::run()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("This benchmark requires Windows.");
    std::process::exit(1);
}
