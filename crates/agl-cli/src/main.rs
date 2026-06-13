use std::env;
use std::io::{self, Write};
use std::process;

use agl_turn::TurnMessage;
use anyhow::{Context, Result};

mod args;
mod session;
mod terminal;

use args::{CliCommand, RunOptions, parse_cli, print_usage};
use session::InferenceSession;
use terminal::assistant_text_for_terminal;

fn main() {
    if let Err(err) = run(env::args()) {
        eprintln!("error: {err:#}");
        process::exit(1);
    }
}

fn run(args: impl IntoIterator<Item = String>) -> Result<()> {
    match parse_cli(args)? {
        CliCommand::Help => {
            print_usage();
            Ok(())
        }
        CliCommand::Infer(options) => run_infer(options),
        CliCommand::Chat(options) => run_chat(options),
    }
}

fn run_infer(options: RunOptions) -> Result<()> {
    let prompt = options
        .prompt
        .clone()
        .context("infer requires --prompt TEXT")?;
    let mut session = InferenceSession::new(options)?;
    let messages = vec![TurnMessage::User { content: prompt }];
    let response = session.generate(&messages, 1)?;
    println!("{}", assistant_text_for_terminal(&response.content));
    Ok(())
}

fn run_chat(options: RunOptions) -> Result<()> {
    let mut session = InferenceSession::new(options)?;
    let mut messages = Vec::new();
    let stdin = io::stdin();
    let mut request_index = 1;

    loop {
        print!("agentLIBRE> ");
        io::stdout().flush().context("failed to flush prompt")?;

        let mut input = String::new();
        let bytes_read = stdin
            .read_line(&mut input)
            .context("failed to read chat input")?;
        if bytes_read == 0 {
            break;
        }

        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if matches!(input, "/exit" | "/quit") {
            break;
        }

        messages.push(TurnMessage::User {
            content: input.to_string(),
        });
        let response = session.generate(&messages, request_index)?;
        let content = assistant_text_for_terminal(&response.content);
        println!("assistant> {content}");
        messages.push(TurnMessage::Assistant { content });
        request_index += 1;
    }

    Ok(())
}
