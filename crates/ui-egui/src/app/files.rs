use super::{EXPORT_SAVE_DIALOGS, FileDialog, FilmcraftApp, Value, json, panels};

impl FilmcraftApp {
    // ---------------------------------------------------------------- files

    /// Destination of imports started from the currently shown Project panel view.
    pub fn import_bin(&self) -> filmcraft_project::BinId {
        panels::project::view_of(self, panels::project::shown_inst(self)).bin
    }

    pub fn file_dialog(&mut self, id: &str, params: &Value) -> Result<Value, String> {
        match id {
            "file.import" => {
                let exts: Vec<&str> = filmcraft_media::VIDEO_EXTENSIONS
                    .iter()
                    .chain(filmcraft_media::AUDIO_EXTENSIONS)
                    .chain(filmcraft_media::STILL_EXTENSIONS)
                    .chain(&["srt", "vtt", "scc", "edl", "xml", "fcpxml", "otio", "aaf", "omf", "prfpset"])
                    .copied()
                    .collect();
                let bin = params.get("bin").cloned();
                self.pick(FileDialog::open_files(&exts), move |app, paths| {
                    if paths.is_empty() {
                        return Ok(Value::Null);
                    }
                    let bin = bin.unwrap_or_else(|| json!(app.import_bin().0));
                    let r = app.session.execute("file.import", json!({"paths": paths, "bin": bin})).map_err(|e| e.to_string());
                    if let Ok(v) = &r {
                        if let Some(errs) = v.get("errors").and_then(Value::as_array)
                            && !errs.is_empty()
                        {
                            app.ui.status = errs.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; ");
                        } else if let Some(presets) = v.get("presets").and_then(Value::as_array)
                            && !presets.is_empty()
                        {
                            if !app.ui.expanded_fx.contains(&"Presets".to_string()) {
                                app.ui.expanded_fx.push("Presets".to_string());
                            }
                            app.ui.status = tlf!("Imported {n} preset(s)", n = presets.len());
                        }
                    }
                    r
                })
            }
            // File ▸ Import with Image Sequence: choose the first numbered still
            "file.importImageSequence" => self.pick(FileDialog::open_files(filmcraft_media::STILL_EXTENSIONS), |app, paths| {
                let Some(path) = paths.into_iter().next() else { return Ok(Value::Null) };
                let r = app.session.execute("file.importImageSequence", json!({"path": path})).map_err(|e| e.to_string());
                if let Err(e) = &r {
                    app.ui.status = e.clone();
                }
                r
            }),
            "file.saveAs" | "file.save" | "file.saveCopy" => {
                let suggested =
                    if id == "file.saveCopy" { format!("{} copy.fcproj", self.session.project.name) } else { format!("{}.fcproj", self.session.project.name) };
                let cmd = if id == "file.saveCopy" { "file.saveCopy" } else { "file.saveAs" };
                self.pick(FileDialog::SaveProject { name: suggested }, move |app, paths| {
                    let Some(path) = paths.into_iter().next() else { return Ok(Value::Null) };
                    let r = app.session.execute(cmd, json!({"path": path})).map_err(|e| e.to_string());
                    match &r {
                        Ok(_) => crate::menus::saved_status(app, cmd, &path),
                        Err(e) => app.ui.status = e.clone(),
                    }
                    r
                })
            }
            // File ▸ New ▸ Project asks where to keep the project first and saves it there, so
            // Auto Save keeps it saved from the first edit. Cancel keeps the current project.
            "file.newProject" => self.pick(FileDialog::SaveProject { name: "Untitled.fcproj".into() }, |app, paths| {
                let Some(path) = paths.into_iter().next() else { return Ok(Value::Null) };
                let name = std::path::Path::new(&path).file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Untitled".into());
                app.session.execute("file.newProject", json!({"name": name})).map_err(|e| e.to_string())?;
                let r = app.session.execute("file.saveAs", json!({"path": path})).map_err(|e| e.to_string());
                match &r {
                    Ok(_) => crate::menus::saved_status(app, "file.saveAs", &path),
                    Err(e) => app.ui.status = e.clone(),
                }
                r
            }),
            "file.open" => self.pick(FileDialog::OpenProject, |app, paths| {
                let Some(path) = paths.into_iter().next() else { return Ok(Value::Null) };
                if let Some(asked) = crate::panels::clip_dialogs::ask_before_open(app, &path) {
                    return asked;
                }
                app.session.execute("file.open", json!({"path": path})).map_err(|e| e.to_string())
            }),
            "graphics.newFromFile" => {
                let exts: Vec<&str> = filmcraft_media::STILL_EXTENSIONS.iter().chain(filmcraft_media::VIDEO_EXTENSIONS).copied().collect();
                self.pick(FileDialog::open_files(&exts), |app, paths| {
                    let Some(path) = paths.into_iter().next() else { return Ok(Value::Null) };
                    app.session.execute("graphics.newFromFile", json!({"path": path})).map_err(|e| e.to_string())
                })
            }
            "captions.import" => self.pick(FileDialog::open_files(&["srt", "vtt", "scc", "mcc", "stl", "ttml", "dfxp", "xml"]), |app, paths| {
                let Some(path) = paths.into_iter().next() else { return Ok(Value::Null) };
                app.session.execute("captions.import", json!({"path": path})).map_err(|e| e.to_string())
            }),
            _ => match EXPORT_SAVE_DIALOGS.iter().find(|(c, ..)| *c == id) {
                Some(&(_, filter, ext)) => self.export_save_dialog(id, filter, ext, params),
                None => Err(format!("no dialog for {id}")),
            },
        }
    }

    /// File ▸ Export ▸ EDL…, Final Cut Pro XML… etc. from the menus (#382): ask where to save, then
    /// run the command with that `path` and the rest of `params`.
    fn export_save_dialog(&mut self, id: &str, filter: &str, ext: &str, params: &Value) -> Result<Value, String> {
        filmcraft_engine::find_command(id).map_or(Ok(()), |c| (c.enabled)(&self.session))?;
        if !FileDialog::save_as(filter, &[ext], "").available(&self.hooks) {
            return Err("no save dialog available: run the command with a `path`".into());
        }
        // Timelines are named after the sequence; a selection must not suggest the open project's file.
        let sequence = self.session.state.active_sequence.and_then(|s| self.session.project.item(s)).map(|i| i.name.clone());
        let stem = match id {
            "file.exportAle" => self.session.project.name.clone(),
            "file.exportSelectionProject" => format!("{} selection", self.session.project.name),
            _ => sequence.unwrap_or_else(|| self.session.project.name.clone()),
        };
        let stem: String = stem.chars().map(|c| if matches!(c, '/' | '\\') || c.is_control() { '-' } else { c }).collect();
        let stem = if stem.trim().is_empty() { "Untitled".to_string() } else { stem };
        let mut p = params.as_object().cloned().unwrap_or_default();
        let id = id.to_string();
        self.pick(FileDialog::save_as(filter, &[ext], &format!("{stem}.{ext}")), move |app, paths| {
            let Some(path) = paths.into_iter().next() else { return Ok(Value::Null) };
            p.insert("path".into(), json!(path));
            let r = app.session.execute(&id, Value::Object(p)).map_err(|e| e.to_string());
            app.ui.status = match &r {
                Ok(_) => tlf!("Exported {path}", path),
                Err(e) => e.clone(),
            };
            r
        })
    }
}

#[cfg(test)]
mod new_project_tests {
    use serde_json::json;

    /// File ▸ New ▸ Project asks where to keep the project and saves it there at once (so Auto Save
    /// keeps saving it); cancelling the dialog keeps the current project.
    #[test]
    fn new_project_is_named_and_saved_first() {
        let dir = std::env::temp_dir().join(format!("fc-newproj-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Mitzpe Ramon.fcproj").to_string_lossy().into_owned();
        let mut s = filmcraft_engine::Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        let mut app = crate::FilmcraftApp::new(s);
        let ctx = egui::Context::default();
        // cancelled: nothing changes
        app.hooks.pick_save = Some(Box::new(|_| None));
        let before = app.session.project.name.clone();
        crate::menus::invoke_command(&mut app, &ctx, "file.newProject", json!({})).unwrap();
        assert_eq!(app.session.project.name, before);
        // named: a new, empty project, saved where chosen
        let chosen = path.clone();
        app.hooks.pick_save = Some(Box::new(move |suggested| {
            assert_eq!(suggested, "Untitled.fcproj");
            Some(chosen.clone())
        }));
        crate::menus::invoke_command(&mut app, &ctx, "file.newProject", json!({})).unwrap();
        assert_eq!(app.session.project.name, "Mitzpe Ramon");
        assert_eq!(app.session.path.as_deref(), Some(path.as_str()));
        assert!(!app.session.is_dirty());
        assert!(std::path::Path::new(&path).is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
