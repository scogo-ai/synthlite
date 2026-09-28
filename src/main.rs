#![forbid(unsafe_code)]

fn main() {
    let code = match synthlite::cli::run() {
        Ok(()) => 0,
        Err(exit) => {
            if !exit.message.is_empty() {
                eprintln!("{}", exit.message);
            }
            exit.code
        }
    };
    std::process::exit(code);
}
