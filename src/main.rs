#![forbid(unsafe_code)]
fn main() {
    let code = match project_check::util::arguments() {
        Ok(arguments) => project_check::cli::main(arguments.into_iter()),
        Err(error) => {
            eprintln!("project-check: error: {error}");
            2
        }
    };
    std::process::exit(code);
}
