fn main() {
    #[cfg(windows)]
    if let Err(error) = dust_indexer::service::run(None) {
        eprintln!("dust-indexer service failed: {error}");
        std::process::exit(1);
    }
    #[cfg(not(windows))]
    eprintln!("dust-indexer service is only available on Windows");
}
