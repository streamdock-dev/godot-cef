use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use cef::{
    Browser, CefStringUtf16, DictionaryValue, Domnode, Frame, ImplBinaryValue, ImplBrowser,
    ImplDictionaryValue, ImplDomnode, ImplFrame, ImplListValue, ImplProcessMessage,
    ImplRenderProcessHandler, ImplV8Context, ImplV8Exception, ImplV8Value, ProcessId,
    ProcessMessage, RenderProcessHandler, V8Context, V8Exception, V8Handler, V8Value,
    WrapRenderProcessHandler, process_message_create, rc::Rc,
    v8_value_create_array_buffer_with_copy, v8_value_create_function, v8_value_create_string,
    wrap_render_process_handler,
};

use crate::ipc_contract::{
    EXTRA_INFO_CUSTOM_CSS, EXTRA_INFO_PRELOAD_SCRIPT, ROUTE_IPC_BINARY_GODOT_TO_RENDERER,
    ROUTE_IPC_DATA_GODOT_TO_RENDERER, ROUTE_IPC_GODOT_TO_RENDERER, ROUTE_TRIGGER_IME,
};
use crate::v8_handlers::{
    OsrImeCaretHandler, OsrImeCaretHandlerBuilder, OsrIpcBinaryHandler, OsrIpcBinaryHandlerBuilder,
    OsrIpcDataHandler, OsrIpcDataHandlerBuilder, OsrIpcHandler, OsrIpcHandlerBuilder,
    build_ipc_listener_object, cbor_bytes_to_v8_value, emit_ipc_listener, v8_prop_default,
};

fn send_browser_bool_message(frame: Option<&mut Frame>, route: &str, value: bool) {
    let Some(frame) = frame else {
        return;
    };
    let route = cef::CefStringUtf16::from(route);
    let Some(mut process_message) = process_message_create(Some(&route)) else {
        return;
    };
    if let Some(argument_list) = process_message.argument_list() {
        argument_list.set_bool(0, value as _);
    }
    frame.send_process_message(ProcessId::BROWSER, Some(&mut process_message));
}

// `extra_info` is only available in `on_browser_created`, while preload runs
// later in `on_context_created`. These callbacks can arrive through different
// Rust wrapper instances, so handler fields cannot carry preload state between
// them. Browser identifiers remain stable across those callbacks.
static PRELOAD_SCRIPTS: LazyLock<Mutex<HashMap<i32, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

// Same lifetime constraint as `PRELOAD_SCRIPTS` above, keyed by the same browser id.
// Re-injected on every `on_context_created` for the main frame, so it survives
// navigations for the life of the browser (matching the property's "applies to every
// page this browser loads" semantics).
static CUSTOM_CSS_STYLES: LazyLock<Mutex<HashMap<i32, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn log_preload_exception(exception: Option<V8Exception>) {
    let Some(exception) = exception else {
        eprintln!("[GodotCef] Preload script failed without exception details");
        return;
    };

    let message = CefStringUtf16::from(&exception.message()).to_string();
    let source_line = CefStringUtf16::from(&exception.source_line()).to_string();
    eprintln!(
        "[GodotCef] Preload script failed at line {}: {}\n{}",
        exception.line_number(),
        message,
        source_line
    );
}

/// Escapes `input` for safe embedding as a double-quoted JS string literal.
/// U+2028/U+2029 are escaped too since they terminate a JS string literal even though
/// they aren't ASCII control characters.
fn escape_js_string(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 2);
    out.push('"');
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Builds a script that appends `css` as a `<style>` element to the document, waiting for
/// `<head>` to exist if the document is still loading. Self-contained (no external state)
/// so it can be re-run on every navigation.
fn build_css_injection_script(css: &str) -> String {
    let escaped_css = escape_js_string(css);
    format!(
        "(function() {{
            var css = {escaped_css};
            function inject() {{
                var head = document.head || document.documentElement;
                if (!head) {{ return; }}
                var style = document.createElement('style');
                style.setAttribute('data-godot-cef-custom-css', '1');
                style.textContent = css;
                head.appendChild(style);
            }}
            if (document.readyState === 'loading') {{
                document.addEventListener('DOMContentLoaded', inject);
            }} else {{
                inject();
            }}
        }})();"
    )
}

fn inject_custom_css(frame: &mut Frame, css: &str) {
    let script: CefStringUtf16 = build_css_injection_script(css).as_str().into();
    frame.execute_java_script(Some(&script), None, 0);
}

fn eval_preload_script(context: &mut V8Context, script: &str) {
    let code: CefStringUtf16 = script.into();
    // Used in dev tools for easier debugging of preload scripts. Not a real URL.
    let script_url: CefStringUtf16 = "godot-cef://user-preload.js".into();
    let mut retval: Option<V8Value> = None;
    let mut exception: Option<V8Exception> = None;

    if context.eval(
        Some(&code),
        Some(&script_url),
        1,
        Some(&mut retval),
        Some(&mut exception),
    ) == 0
    {
        log_preload_exception(exception);
    }
}

#[derive(Clone)]
pub(crate) struct OsrRenderProcessHandler;

impl OsrRenderProcessHandler {
    pub fn new() -> Self {
        Self
    }
}

wrap_render_process_handler! {
    pub(crate) struct RenderProcessHandlerBuilder {
        handler: OsrRenderProcessHandler,
    }

    impl RenderProcessHandler {
        fn on_browser_created(
            &self,
            browser: Option<&mut Browser>,
            extra_info: Option<&mut DictionaryValue>,
        ) {
            let Some(browser) = browser else {
                return;
            };
            let browser_id = browser.identifier();

            if let Ok(mut preload_scripts) = PRELOAD_SCRIPTS.lock() {
                preload_scripts.remove(&browser_id);
            } else {
                eprintln!("[GodotCef] preload cache lock failed in on_browser_created");
            }
            if let Ok(mut custom_css_styles) = CUSTOM_CSS_STYLES.lock() {
                custom_css_styles.remove(&browser_id);
            } else {
                eprintln!("[GodotCef] custom css cache lock failed in on_browser_created");
            }

            let Some(extra_info) = extra_info else {
                return;
            };

            let preload_key: CefStringUtf16 = EXTRA_INFO_PRELOAD_SCRIPT.into();
            if extra_info.has_key(Some(&preload_key)) != 0 {
                let script = CefStringUtf16::from(&extra_info.string(Some(&preload_key))).to_string();
                if !script.is_empty()
                    && let Ok(mut preload_scripts) = PRELOAD_SCRIPTS.lock()
                {
                    preload_scripts.insert(browser_id, script);
                }
            }

            let css_key: CefStringUtf16 = EXTRA_INFO_CUSTOM_CSS.into();
            if extra_info.has_key(Some(&css_key)) != 0 {
                let css = CefStringUtf16::from(&extra_info.string(Some(&css_key))).to_string();
                if !css.is_empty()
                    && let Ok(mut custom_css_styles) = CUSTOM_CSS_STYLES.lock()
                {
                    custom_css_styles.insert(browser_id, css);
                }
            }
        }

        fn on_context_created(&self, browser: Option<&mut Browser>, frame: Option<&mut Frame>, context: Option<&mut V8Context>) {
            let Some(context) = context else {
                return;
            };
            let Some(global) = context.global() else {
                return;
            };
            let Some(frame) = frame else {
                return;
            };
            let frame_arc = Arc::new(Mutex::new(frame.clone()));

            register_v8_function(&global, "sendIpcMessage",
                &mut OsrIpcHandlerBuilder::build(OsrIpcHandler::new(Some(frame_arc.clone()))));
            register_v8_function(&global, "sendIpcBinaryMessage",
                &mut OsrIpcBinaryHandlerBuilder::build(OsrIpcBinaryHandler::new(Some(frame_arc.clone()))));
            register_v8_function(&global, "sendIpcData",
                &mut OsrIpcDataHandlerBuilder::build(OsrIpcDataHandler::new(Some(frame_arc.clone()))));

            for name in ["ipcMessage", "ipcBinaryMessage", "ipcDataMessage"] {
                if let Some(mut obj) = build_ipc_listener_object() {
                    register_v8_value(&global, name, &mut obj);
                }
            }

            register_v8_function(&global, "__sendImeCaretPosition",
                &mut OsrImeCaretHandlerBuilder::build(OsrImeCaretHandler::new(Some(frame_arc))));

            let helper_script: cef::CefStringUtf16 = include_str!("ime_helper.js").into();
            frame.execute_java_script(Some(&helper_script), None, 0);

            if frame.is_main() == 0 {
                return;
            }
            let Some(browser) = browser else {
                return;
            };
            let browser_id = browser.identifier();
            let preload_script = PRELOAD_SCRIPTS
                .lock()
                .ok()
                .and_then(|scripts| scripts.get(&browser_id).cloned());
            if let Some(preload_script) = preload_script {
                eval_preload_script(context, &preload_script);
            }

            let custom_css = CUSTOM_CSS_STYLES
                .lock()
                .ok()
                .and_then(|styles| styles.get(&browser_id).cloned());
            if let Some(custom_css) = custom_css {
                inject_custom_css(frame, &custom_css);
            }
        }

        fn on_browser_destroyed(&self, browser: Option<&mut Browser>) {
            let Some(browser) = browser else {
                return;
            };
            let browser_id = browser.identifier();
            if let Ok(mut preload_scripts) = PRELOAD_SCRIPTS.lock() {
                preload_scripts.remove(&browser_id);
            } else {
                eprintln!("[GodotCef] preload cache lock failed in on_browser_destroyed");
            }
            if let Ok(mut custom_css_styles) = CUSTOM_CSS_STYLES.lock() {
                custom_css_styles.remove(&browser_id);
            } else {
                eprintln!("[GodotCef] custom css cache lock failed in on_browser_destroyed");
            }
        }

        fn on_focused_node_changed(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, node: Option<&mut Domnode>) {
            if let Some(node) = node
                && node.is_editable() == 1 {
                    if let Some(frame) = frame {
                        // send to the browser process to activate IME
                        send_browser_bool_message(Some(frame), ROUTE_TRIGGER_IME, true);
                        let report_script: cef::CefStringUtf16 = "if(window.__activateImeTracking)window.__activateImeTracking();".into();
                        frame.execute_java_script(Some(&report_script), None, 0);
                    }
                    return;
                }

            if let Some(frame) = frame {
                // send to the browser process to deactivate IME
                send_browser_bool_message(Some(frame), ROUTE_TRIGGER_IME, false);
                let deactivate_script: cef::CefStringUtf16 = "if(window.__deactivateImeTracking)window.__deactivateImeTracking();".into();
                frame.execute_java_script(Some(&deactivate_script), None, 0);
            }
        }

        fn on_process_message_received(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> i32 {
            let Some(message) = message else { return 0 };
            let route = CefStringUtf16::from(&message.name()).to_string();

            match route.as_str() {
                ROUTE_IPC_GODOT_TO_RENDERER => {
                    if let Some(args) = message.argument_list()
                        && let Some(frame) = frame
                    {
                        let msg_cef = args.string(0);
                        let msg_str = CefStringUtf16::from(&msg_cef);
                        invoke_js_callback(frame, "onIpcMessage", "ipcMessage", |_| {
                            v8_value_create_string(Some(&msg_str))
                        });
                    }
                    return 1;
                }
                ROUTE_IPC_BINARY_GODOT_TO_RENDERER => {
                    if let Some(buffer) = extract_binary_payload(message)
                        && let Some(frame) = frame
                    {
                        invoke_js_callback(frame, "onIpcBinaryMessage", "ipcBinaryMessage", |_| {
                            let mut copy = buffer.clone();
                            v8_value_create_array_buffer_with_copy(copy.as_mut_ptr(), copy.len())
                        });
                    }
                    return 1;
                }
                ROUTE_IPC_DATA_GODOT_TO_RENDERER => {
                    if let Some(buffer) = extract_binary_payload(message)
                        && let Some(frame) = frame
                    {
                        invoke_js_callback(frame, "onIpcDataMessage", "ipcDataMessage", |_| {
                            cbor_bytes_to_v8_value(&buffer).ok()
                        });
                    }
                    return 1;
                }
                _ => {}
            }

            0
        }
    }
}

fn register_v8_function(global: &V8Value, name: &str, handler: &mut V8Handler) {
    let key: CefStringUtf16 = name.into();
    let Some(mut func) = v8_value_create_function(Some(&key), Some(handler)) else {
        eprintln!("[godot-cef] Failed to create V8 function for '{name}'");
        return;
    };
    global.set_value_bykey(Some(&key), Some(&mut func), v8_prop_default());
}

fn register_v8_value(global: &V8Value, name: &str, value: &mut V8Value) {
    let key: CefStringUtf16 = name.into();
    global.set_value_bykey(Some(&key), Some(value), v8_prop_default());
}

fn extract_binary_payload(message: &mut ProcessMessage) -> Option<Vec<u8>> {
    let args = message.argument_list()?;
    let binary_value = args.binary(0)?;
    let size = binary_value.size();
    if size == 0 {
        return None;
    }
    let mut buffer = vec![0u8; size];
    let copied = binary_value.data(Some(&mut buffer), 0);
    if copied == 0 {
        return None;
    }
    buffer.truncate(copied);
    Some(buffer)
}

fn invoke_js_callback(
    frame: &mut Frame,
    callback_name: &str,
    listener_api_name: &str,
    create_value: impl FnOnce(&mut V8Value) -> Option<V8Value>,
) {
    if let Some(context) = frame.v8_context()
        && context.enter() != 0
    {
        if let Some(mut global) = context.global()
            && let Some(value) = create_value(&mut global)
        {
            let callback_key: CefStringUtf16 = callback_name.into();
            if let Some(callback) = global.value_bykey(Some(&callback_key))
                && callback.is_function() != 0
            {
                let args = [Some(value.clone())];
                let _ = callback.execute_function(Some(&mut global), Some(&args));
            }
            let listener_api_key: CefStringUtf16 = listener_api_name.into();
            if let Some(mut listener_api) = global.value_bykey(Some(&listener_api_key)) {
                emit_ipc_listener(&mut listener_api, &mut global, &value);
            }
        }
        context.exit();
    }
}

impl RenderProcessHandlerBuilder {
    pub(crate) fn build(handler: OsrRenderProcessHandler) -> RenderProcessHandler {
        Self::new(handler)
    }
}
