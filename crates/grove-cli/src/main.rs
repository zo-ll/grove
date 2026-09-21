fn main() {
    std::process::exit(grove_cli::run(std::env::args_os().skip(1)));
}
