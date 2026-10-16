use clap::Parser;

fn main() {
    let cli = shunt::cli::Cli::parse();
    if let Err(e) = shunt::cli::run(cli) {
        eprintln!("shunt: {e:#}");
        std::process::exit(1);
    }
}
