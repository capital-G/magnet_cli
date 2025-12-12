// @TODO remove later
#![allow(dead_code)]

mod cli;

use cli::cli;

#[tokio::main]
async fn main() {
    match cli().await {
        Ok(_) => {}
        Err(err) => {
            eprintln!("Error: {}", err);
            for cause in err.chain().skip(1) {
                eprintln!("Caused by:\n\t{}", cause);
            }
            std::process::exit(1);
        }
    }
}
