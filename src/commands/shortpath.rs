#![allow(clippy::expect_used)]

use clap::{Arg, ArgAction, ArgGroup, Command};
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
        .arg(
            Arg::new("project")
                .long("project")
                .help("Output only the project/repo prefix segment (symbol + name)")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("dir")
                .long("path")
                .help("Output only the directory path segment (infix + suffix, no prefix)")
                .action(ArgAction::SetTrue),
        )
        .group(
            ArgGroup::new("output_mode")
                .args(["project", "dir"])
                .required(false),
        )
}

pub fn handle(matches: &clap::ArgMatches) {
    let path = matches.get_one::<String>("path").expect("path is required");
    let path_to_shorten = expand_path(path);
    let short_path = shorten_path(&path_to_shorten);

    let output = if matches.get_flag("project") {
        short_path.build(1, &[Prefix])
    } else if matches.get_flag("dir") {
        short_path.build(1, &[Infix, Suffix])
    } else {
        short_path.build(1, &[Prefix, Infix, Suffix])
    };

    println!("{output}");
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
