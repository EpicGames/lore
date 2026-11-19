// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! The application window.

use std::sync::mpsc;

use eframe::egui;
use egui_extras::Column;
use egui_extras::TableBuilder;

use crate::status::Progress;
use crate::status::Section;
use crate::status::StatusMessage;
use crate::status::StatusModel;
use crate::status::action_label;
use crate::worker::StatusRequest;
use crate::worker::spawn_status;

const ROW_HEIGHT: f32 = 18.0;

/// The desktop client's window: one repository instance and its status.
pub struct LoreDesktop {
    instance_path: String,
    status: StatusModel,
    /// Messages of the running status call, sent from the library's threads.
    messages: Option<mpsc::Receiver<StatusMessage>>,
}

impl LoreDesktop {
    pub fn new(instance_path: Option<String>) -> Self {
        Self {
            instance_path: instance_path.unwrap_or_default(),
            status: StatusModel::default(),
            messages: None,
        }
    }

    fn refresh(&mut self, ctx: &egui::Context, scan: bool) {
        if self.status.is_running() || self.instance_path.trim().is_empty() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let repaint = ctx.clone();
        let request = StatusRequest {
            instance_path: self.instance_path.trim().to_owned(),
            scan,
        };
        self.status.start();
        let spawned = spawn_status(request, move |message| {
            // The window may have closed; nothing is left to show the message then.
            let _ = sender.send(message);
            repaint.request_repaint();
        });
        match spawned {
            Ok(_) => self.messages = Some(receiver),
            Err(err) => self.status.apply(StatusMessage::Complete(Err(format!(
                "could not start a thread: {err}"
            )))),
        }
    }

    fn drain_messages(&mut self) {
        let Some(receiver) = &self.messages else {
            return;
        };
        let mut finished = false;
        loop {
            match receiver.try_recv() {
                Ok(message) => {
                    finished |= matches!(message, StatusMessage::Complete(_));
                    self.status.apply(message);
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if self.status.is_running() {
                        self.status.apply(StatusMessage::Complete(Err(
                            "the status call ended without reporting".to_owned(),
                        )));
                    }
                    finished = true;
                    break;
                }
            }
        }
        if finished {
            self.messages = None;
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Instance");
            let path = ui.add(
                egui::TextEdit::singleline(&mut self.instance_path)
                    .hint_text("path to a working tree")
                    .desired_width(ui.available_width() - 180.0),
            );
            let entered = path.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let idle = !self.status.is_running();
            let refresh = ui.add_enabled(idle, egui::Button::new("Refresh")).clicked();
            let scan = ui
                .add_enabled(idle, egui::Button::new("Scan"))
                .on_hover_text("Walk the working tree for changes made outside Lore")
                .clicked();
            if entered || refresh || scan {
                self.refresh(ui.ctx(), scan);
            }
        });
    }

    fn summary(&self, ui: &mut egui::Ui) {
        match &self.status.progress {
            Progress::Idle => {
                ui.label("Enter the path of a repository instance and press Refresh.");
            }
            Progress::Running => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Reading status…");
                });
            }
            Progress::Failed(message) => {
                ui.colored_label(ui.visuals().error_fg_color, message);
            }
            Progress::Done => {}
        }
        if let Some(revision) = &self.status.revision {
            ui.horizontal_wrapped(|ui| {
                ui.strong(&revision.branch);
                ui.label(format!("revision {}", revision.revision_number));
                ui.weak(&revision.revision);
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(revision.remote.label());
                if let Some(incoming) = &revision.pending_merge {
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        format!("Merge in progress, incoming {incoming}"),
                    );
                }
            });
        }
    }

    fn files(&self, ui: &mut egui::Ui) {
        let sections: Vec<Section> = Section::ALL
            .into_iter()
            .filter(|section| !self.status.files(*section).is_empty())
            .collect();
        if sections.is_empty() {
            if self.status.progress == Progress::Done {
                ui.label("No changes.");
            }
            return;
        }
        // Each section gets an even share of the height, and its table draws only the rows in
        // view, so a status of any size costs the same to draw.
        let share = ui.available_height() / sections.len() as f32;
        for section in sections {
            let files = self.status.files(section);
            ui.push_id(section.title(), |ui| {
                ui.strong(format!("{} ({})", section.title(), files.len()));
                TableBuilder::new(ui)
                    .striped(true)
                    .max_scroll_height(share - 3.0 * ROW_HEIGHT)
                    .column(Column::exact(80.0))
                    .column(Column::remainder())
                    .column(Column::exact(90.0))
                    .header(ROW_HEIGHT, |mut header| {
                        header.col(|ui| {
                            ui.weak("Change");
                        });
                        header.col(|ui| {
                            ui.weak("Path");
                        });
                        header.col(|ui| {
                            ui.weak("Size");
                        });
                    })
                    .body(|body| {
                        body.rows(ROW_HEIGHT, files.len(), |mut row| {
                            let file = &files[row.index()];
                            row.col(|ui| {
                                ui.label(action_label(file.action));
                            });
                            row.col(|ui| match &file.from_path {
                                Some(from) => {
                                    ui.label(format!("{from} → {}", file.path));
                                }
                                None => {
                                    ui.label(&file.path);
                                }
                            });
                            row.col(|ui| {
                                ui.label(format!("{} B", file.size));
                            });
                        });
                    });
            });
        }
    }
}

impl eframe::App for LoreDesktop {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain_messages();
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            self.toolbar(ui);
            ui.add_space(4.0);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            self.summary(ui);
            ui.separator();
            self.files(ui);
        });
    }
}
