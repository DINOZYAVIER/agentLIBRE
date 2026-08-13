fn main() {
    if let Err(error) = agl_terminald::run_from_environment() {
        eprintln!("agl-terminald: {error}");
        std::process::exit(1);
    }
}
