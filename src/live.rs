//! The open window as the tools' host, shared by the desktop control port (`remote.rs`) and the
//! web page's `window.anicraft` (`web_api.rs`): an assistant's call runs on the window's own
//! session, with the follow-up the person's own commands get.

use crate::app::{App, Drag};
use anicraft_cli::mcp;
use anicraft_engine::Session;
use serde_json::Value;

/// Whether the person is in the middle of an edit a remote one must not join.
pub(crate) fn person_is_dragging(app: &App, ctx: &egui::Context) -> bool {
    app.drag != Drag::None || app.plugin_dialog.is_some() || app.audio_keys.is_some() || (app.s.in_gesture() && ctx.egui_is_using_pointer())
}

/// The window as the tools' host: commands run with the follow-up the person's own get (the
/// status line, the view a new document resets, Open Recent).
pub(crate) struct Live<'a>(pub &'a mut App);

impl mcp::Host for Live<'_> {
    fn session(&self) -> &Session {
        &self.0.s
    }
    fn execute(&mut self, id: &str, params: &Value) -> Result<Value, String> {
        let selected = self.0.s.selection.len();
        let revision = self.0.s.revision;
        let result = self.0.s.execute(id, params);
        if crate::platform::WEB && id.starts_with("file.") {
            // In a browser a save or an export wrote to memory: hand it over, as `App::run` does.
            crate::platform::deliver(anicraft_engine::vfs::take_written());
        }
        let v = result.map_err(|e| e.to_string())?;
        // An edit to the document behind the start screen: show the document, so the person
        // sees it land (ADR 0059's start screen otherwise covers the workspace).
        if self.0.s.revision != revision {
            self.0.start = None;
        }
        // The window's own follow-up (its settings, Open Recent) is not the assistant's file
        // access: the automation roots apply to the command above only (ADR 0124).
        anicraft_engine::access::unconfined(|| self.0.announce(id, &v, selected));
        Ok(v)
    }
}
