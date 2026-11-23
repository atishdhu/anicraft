//! `window.anicraft`: the page's command API in the web build, the same protocol as the desktop
//! control port (`docs/control-protocol.md`): `hello`, `tools.list` and `tools.call` run the MCP
//! tools on the open window, and `craft-web-api`'s shared functions (`execute`, `commands`,
//! `docInspect`) are its tools under the names every Craft web build uses.
//!
//! ```js
//! await anicraft.request("tools.call", {name: "anicraft_run", arguments: {command: "draw.rect", params: {x: 0, y: 0, w: 50, h: 50}}})
//! await anicraft.execute("draw.rect", {x: 0, y: 0, w: 50, h: 50})  // the command's result
//! await anicraft.docInspect({})                                   // anicraft_document
//! anicraft.openFile("scene.anicraft", bytes)                       // opened as a picked file is
//! anicraft.onPickFile((cmd, exts) => …); anicraft.pickedFile(name, bytes)  // the page answers file prompts
//! anicraft.captureDownloads(true); anicraft.files(); anicraft.readFile("scene.anicraft")  // saves stay in the page
//! ```
//!
//! A call runs at the start of a frame, as a remote-control call does (`remote.rs`): an edit waits
//! while the person drags, and is its own undo step.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use anicraft_cli::mcp;
use craft_web_api::methods::Methods;
use craft_web_api::page::{PageApi, Request};
use serde_json::{Value, json};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use crate::app::App;
use crate::live::{Live, person_is_dragging};

/// Kept saves, newest last; the oldest go past this many.
const MAX_SAVED: usize = 16;
/// How long a page call may wait for the frame loop, as the control port's 60 s.
const TIMEOUT_MS: u32 = 60_000;

#[derive(Default)]
struct Slot {
    reply: RefCell<Option<Value>>,
    waker: RefCell<Option<Waker>>,
    /// The page stopped waiting (its timeout): the call must not run.
    dropped: Cell<bool>,
}

struct Call {
    name: String,
    arguments: Value,
    slot: Rc<Slot>,
}

/// A call's reply; dropping it before the reply gives the call up.
struct Reply(Rc<Slot>);

impl Future for Reply {
    type Output = Value;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Value> {
        if let Some(v) = self.0.reply.borrow_mut().take() {
            return Poll::Ready(v);
        }
        *self.0.waker.borrow_mut() = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        self.0.dropped.set(true);
    }
}

thread_local! {
    static QUEUE: RefCell<VecDeque<Call>> = const { RefCell::new(VecDeque::new()) };
    static CAPTURE: Cell<bool> = const { Cell::new(false) };
    static SAVED: RefCell<Vec<(String, Vec<u8>)>> = const { RefCell::new(Vec::new()) };
    static PICK_HANDLER: RefCell<Option<js_sys::Function>> = const { RefCell::new(None) };
    static PENDING_PICK: Cell<Option<&'static str>> = const { Cell::new(None) };
}

fn answer(slot: &Slot, v: Value) {
    *slot.reply.borrow_mut() = Some(v);
    if let Some(w) = slot.waker.borrow_mut().take() {
        w.wake();
    }
}

fn reply_of(result: Result<Value, String>) -> Value {
    match result {
        Ok(v) => json!({"ok": true, "result": v}),
        Err(e) => json!({"ok": false, "error": e}),
    }
}

fn ready(v: Value) -> Pin<Box<dyn Future<Output = Value>>> {
    Box::pin(std::future::ready(v))
}

/// Queue a tool call for the frame loop; it resolves with the protocol's reply object.
fn queue(name: &str, arguments: Value) -> Pin<Box<dyn Future<Output = Value>>> {
    let slot = Rc::new(Slot::default());
    QUEUE.with(|q| q.borrow_mut().push_back(Call { name: name.to_string(), arguments, slot: slot.clone() }));
    crate::web::repaint();
    Box::pin(Reply(slot))
}

/// A tool's MCP result as the plain value its text holds, for the shared functions.
fn unwrap_tool(reply: Value) -> Value {
    if reply.get("ok").and_then(Value::as_bool) != Some(true) {
        return reply;
    }
    let result = reply.get("result").cloned().unwrap_or(Value::Null);
    let text = result.pointer("/content/0/text").and_then(Value::as_str).unwrap_or("");
    let v = serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()));
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let error = v.as_str().map_or_else(|| v.to_string(), str::to_string);
        return json!({"ok": false, "error": error});
    }
    json!({"ok": true, "result": v})
}

fn send(method: &str, params: Value) -> Pin<Box<dyn Future<Output = Value>>> {
    match method {
        "hello" => ready(json!({"ok": true, "result": {"app": "AniCraft", "version": env!("CARGO_PKG_VERSION"), "protocol": 2}})), // i18n-exempt: protocol
        "tools.list" => ready(json!({"ok": true, "result": mcp::tools()})),
        "tools.call" => match params.get("name").and_then(Value::as_str) {
            Some(name) => queue(name, params.get("arguments").cloned().unwrap_or_else(|| json!({}))),
            None => ready(json!({"ok": false, "error": "tools.call needs a tool \"name\" in params"})), // i18n-exempt: protocol
        },
        "engine.execute" => {
            let call = queue("anicraft_run", json!({"command": params.get("command"), "params": params.get("params").cloned().unwrap_or_else(|| json!({}))}));
            Box::pin(async move { unwrap_tool(call.await) })
        }
        "engine.commands" => {
            let call = queue("anicraft_commands", params);
            Box::pin(async move { unwrap_tool(call.await) })
        }
        "document.inspect" => {
            let call = queue("anicraft_document", params);
            Box::pin(async move { unwrap_tool(call.await) })
        }
        other => ready(json!({"ok": false, "error": format!("unknown method '{other}' (hello, tools.list, tools.call)")})), // i18n-exempt: protocol
    }
}

/// Runs the page's waiting calls, at the start of a frame (as `remote::poll`).
pub fn poll(app: &mut App, ctx: &egui::Context) {
    loop {
        let dragging = person_is_dragging(app, ctx);
        let Some(call) = QUEUE.with(|q| {
            let mut q = q.borrow_mut();
            let next = q.front()?;
            if !next.slot.dropped.get() && dragging && mcp::tool_changes_document(&next.name) {
                return None;
            }
            q.pop_front()
        }) else {
            if QUEUE.with(|q| !q.borrow().is_empty()) {
                // The drag ends without a wake-up from the page: look again shortly.
                ctx.request_repaint_after(std::time::Duration::from_millis(50));
            }
            return;
        };
        if call.slot.dropped.get() {
            continue;
        }
        if mcp::tool_changes_document(&call.name) && app.s.in_gesture() {
            // A typed property change still open: it becomes its own undo step first.
            app.s.end_gesture();
        }
        // The page's files are the browser's in-memory store: there is no disk to confine.
        let result = mcp::call_tool_in(&mut Live(app), &call.name, &call.arguments, None);
        answer(&call.slot, reply_of(result));
    }
}

/// Keep a save instead of downloading it, when the page asked for that. True when kept.
pub fn capture(name: &str, bytes: &[u8]) -> bool {
    if !CAPTURE.with(Cell::get) {
        return false;
    }
    SAVED.with(|s| {
        let mut s = s.borrow_mut();
        s.retain(|(n, _)| n != name);
        s.push((name.to_string(), bytes.to_vec()));
        let extra = s.len().saturating_sub(MAX_SAVED);
        s.drain(..extra);
    });
    true
}

/// Hand a file prompt to the page, when it asked for them. True when the page takes it.
pub fn pick(cmd: &'static str, exts: &[&str]) -> bool {
    PICK_HANDLER.with(|h| {
        let Some(f) = h.borrow().clone() else { return false };
        PENDING_PICK.with(|p| p.set(Some(cmd)));
        let list: js_sys::Array = exts.iter().map(|e| JsValue::from_str(e)).collect();
        f.call2(&JsValue::NULL, &JsValue::from_str(cmd), &list).is_ok()
    })
}

fn to_js(v: &Value) -> JsValue {
    js_sys::JSON::parse(&v.to_string()).unwrap_or(JsValue::NULL)
}

fn set_fn(obj: &js_sys::Object, name: &str, f: JsValue) -> Result<(), JsValue> {
    js_sys::Reflect::set(obj, &name.into(), &f).map(|_| ())
}

/// Install `window.anicraft`: the shared functions, then AniCraft's own.
pub fn install() -> Result<(), JsValue> {
    let request: Request = Rc::new(|method, params| send(&method, params));
    let methods = Methods { commands: Some("engine.commands"), execute: Some("engine.execute"), doc_inspect: Some("document.inspect"), ui_inspect: None, screenshot: None };
    Rc::new(PageApi::new("anicraft", methods, request).with_timeout(TIMEOUT_MS)).install()?;
    let window = web_sys::window().ok_or("no window")?; // i18n-exempt: console
    let obj: js_sys::Object = js_sys::Reflect::get(&window, &"anicraft".into())?.unchecked_into();
    set_fn(
        &obj,
        "openFile",
        Closure::<dyn Fn(String, js_sys::Uint8Array)>::new(|name: String, bytes: js_sys::Uint8Array| crate::web::deliver_picked("file.open", name, Ok(bytes.to_vec()))).into_js_value(),
    )?;
    set_fn(
        &obj,
        "onPickFile",
        Closure::<dyn Fn(JsValue)>::new(|f: JsValue| PICK_HANDLER.with(|h| *h.borrow_mut() = f.dyn_into::<js_sys::Function>().ok())).into_js_value(),
    )?;
    set_fn(
        &obj,
        "pickedFile",
        Closure::<dyn Fn(String, js_sys::Uint8Array) -> bool>::new(|name: String, bytes: js_sys::Uint8Array| {
            let Some(cmd) = PENDING_PICK.with(Cell::take) else { return false };
            crate::web::deliver_picked(cmd, name, Ok(bytes.to_vec()));
            true
        })
        .into_js_value(),
    )?;
    set_fn(&obj, "captureDownloads", Closure::<dyn Fn(bool)>::new(|on: bool| CAPTURE.with(|c| c.set(on))).into_js_value())?;
    set_fn(
        &obj,
        "files",
        Closure::<dyn Fn() -> JsValue>::new(|| SAVED.with(|s| to_js(&Value::Array(s.borrow().iter().map(|(n, b)| json!({"path": n, "size": b.len()})).collect()))))
            .into_js_value(),
    )?;
    set_fn(
        &obj,
        "readFile",
        Closure::<dyn Fn(String) -> Result<JsValue, JsValue>>::new(|name: String| {
            let wanted = name.rsplit(['/', '\\']).next().unwrap_or(&name).to_string();
            SAVED.with(|s| {
                s.borrow()
                    .iter()
                    .find(|(n, _)| *n == wanted)
                    .map(|(_, b)| js_sys::Uint8Array::from(b.as_slice()).into())
                    .ok_or_else(|| js_sys::Error::new(&format!("{name}: not found")).into()) // i18n-exempt: console
            })
        })
        .into_js_value(),
    )?;
    Ok(())
}
