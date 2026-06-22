// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::cell::RefCell;
use std::io::stdin;
use std::rc::Rc;
use std::time::Instant;

use tracing::error_span;
use tracing::info;

use crate::chaos::config::RunnerConfig;
use crate::chaos::performer::Performer;
use crate::chaos::picker::JsonPicker;
use crate::chaos::picker::Picker;
use crate::chaos::picker::RandomPicker;
use crate::lore::interface::LoreInterface;
use crate::operations::writer::OperationWriter;
use crate::probability::ProbabilityEngine;

pub struct ChaosRunner {
    config: Rc<RunnerConfig>,
    picker: Box<dyn Picker>,
    performer: Performer,
}

impl ChaosRunner {
    pub fn new(config: RunnerConfig, probability: ProbabilityEngine, urc: LoreInterface) -> Self {
        let config = Rc::new(config);
        let probability = Rc::new(RefCell::new(probability));
        let picker: Box<dyn Picker> = if let Some(replay_file) = config.replay_operations.as_ref() {
            Box::new(JsonPicker::new(replay_file))
        } else {
            Box::new(RandomPicker::new(probability.clone()))
        };
        Self {
            picker,
            performer: Performer::new(config.clone(), urc, probability),
            config,
        }
    }

    pub fn run(&mut self) {
        if !self.config.dry_run {
            LoreInterface::setup();
        }
        let mut operation_writer = self
            .config
            .write_operations
            .as_ref()
            .map(OperationWriter::new);

        let start = Instant::now();
        let mut i = 0;
        'iterations: loop {
            i += 1;
            let duration = Instant::now() - start;
            let past_time_limit = self
                .config
                .time_limit
                .map(|limit| duration >= limit)
                .unwrap_or_default();
            let past_iteration_limit = self
                .config
                .iterations
                .map(|limit| i > limit)
                .unwrap_or_default();
            if past_time_limit || past_iteration_limit {
                info!(?duration, iteration_number = i, "Ending");
                break 'iterations;
            }

            let Some((operation, context)) = self.picker.pick_repo_operation() else {
                break 'iterations;
            };
            let _iteration_span = error_span!("run_iteration", number = i).entered();
            info!(?operation, ?context, "picked operation");

            if let Some(operation_writer) = operation_writer.as_mut() {
                operation_writer.add_operation(operation.clone());
            }

            if !self.performer.perform_repo_operation(operation) {
                break 'iterations;
            }
            if self.config.user_progressed {
                println!("Press enter to continue");
                let mut buffer = String::new();
                stdin().read_line(&mut buffer).unwrap();
            }
        }

        if let Some(operation_writer) = operation_writer {
            operation_writer.write_to_file();
        }
    }
}
