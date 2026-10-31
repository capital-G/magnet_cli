// @TODO remove later
#![allow(dead_code)]

mod cli;

use anstyle::{AnsiColor, Style};
use cli::cli;
use magnet::error::HintedError;

#[tokio::main]
async fn main() {
    match cli().await {
        Ok(_) => {}
        Err(err) => {
            print_error(&err);
            std::process::exit(1);
        }
    }
}

/// `error: <message>`, each cause indented below it, and the hints of a [HintedError] last.
/// Colours are dropped if stderr isn't a terminal.
fn print_error(err: &anyhow::Error) {
    let red = Style::new().bold().fg_color(Some(AnsiColor::Red.into()));
    let cyan = Style::new().bold().fg_color(Some(AnsiColor::Cyan.into()));
    let indented = |message: &str| {
        message
            .lines()
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let message = err.to_string();
    let (first, rest) = message.split_once('\n').unwrap_or((&message, ""));
    anstream::eprintln!("{red}error:{red:#} {first}");
    if !rest.is_empty() {
        anstream::eprintln!("{}", indented(rest));
    }
    for cause in err.chain().skip(1) {
        anstream::eprintln!("\nCaused by:\n{}", indented(&cause.to_string()));
    }

    let hints: Vec<&String> = err
        .chain()
        .filter_map(|cause| cause.downcast_ref::<HintedError>())
        .flat_map(|hinted| &hinted.hints)
        .collect();
    if !hints.is_empty() {
        anstream::eprintln!();
    }
    for hint in hints {
        anstream::eprintln!("{cyan}hint:{cyan:#} {hint}");
    }
}
