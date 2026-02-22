#![allow(clippy::expect_used)]

use clap::{Arg, ArgAction, Command};
use jumpr::shorten_path;
use jumpr::ShortPathPart::*;
use std::env;
use std::path::PathBuf;

pub fn command() -> Command {
    Command::new("shortpath")
        .about("Shortens paths for shell prompts")
        .arg(
            Arg::new("path")
                .value_name("PATH")
                .help("Path to shorten (default: current directory)")
                .default_value(".")
                .action(ArgAction::Set)
                .required(false),
        )
}

pub fn handle(matches: &clap::ArgMatches) {
    let path = matches.get_one::<String>("path").expect("path is required");
    let path_to_shorten = expand_path(path);
    let short_path = shorten_path(&path_to_shorten);
    println!("{}", short_path.build(1, &[Prefix, Infix, Suffix]));
}

fn expand_path(path: &str) -> PathBuf {
    let path_buf = if path == "." {
        env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(path)
    };

    if let Ok(canonical) = path_buf.canonicalize() {
        canonical
    } else {
        path_buf
    }
}
