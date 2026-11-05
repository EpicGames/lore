// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Command line arguments, as `store_stress` takes them: positionals, `--name value` or
//! `--name=value` options, and bare `--flag`s.

use std::collections::HashMap;

pub struct Args {
    pub positional: Vec<String>,
    options: HashMap<String, String>,
}

impl Args {
    pub fn parse(args: impl Iterator<Item = String>) -> Args {
        let mut positional = vec![];
        let mut options = HashMap::new();
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            let Some(name) = arg.strip_prefix("--") else {
                positional.push(arg);
                continue;
            };
            if let Some((name, value)) = name.split_once('=') {
                options.insert(name.to_string(), value.to_string());
            } else if args.peek().is_some_and(|next| !next.starts_with("--")) {
                options.insert(name.to_string(), args.next().unwrap_or_default());
            } else {
                options.insert(name.to_string(), "true".to_string());
            }
        }
        Args {
            positional,
            options,
        }
    }

    /// The names of the options given.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.options.keys().map(String::as_str)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.options.get(name).map(String::as_str)
    }

    /// The number `name` gives, `None` where it is not given, and `Err` where it is not a number.
    pub fn number(&self, name: &str) -> Result<Option<u64>, String> {
        self.get(name)
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|error| format!("--{name} takes a number, not '{value}': {error}"))
            })
            .transpose()
    }

    pub fn flag(&self, name: &str) -> bool {
        self.get(name)
            .is_some_and(|value| matches!(value, "true" | "1" | "yes"))
    }
}
