#[path = "../../protocol/src/lib.rs"]
#[allow(dead_code)]
mod protocol;

mod helper;

fn main() {
    pluribus_log::init();
    if let Err(error) = helper::run(std::env::args().skip(1)) {
        eprintln!("pluribus-shell-cli: {error}");
        std::process::exit(2);
    }
}
