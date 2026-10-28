use std::cell::RefCell;

use cef::{
    Browser, BrowserProcessHandler, Client, CommandLine, ImplBrowser, ImplBrowserHost,
    ImplBrowserProcessHandler, ImplClient, ImplCommandLine, ImplLifeSpanHandler, LifeSpanHandler,
    WrapBrowserProcessHandler, WrapClient, WrapLifeSpanHandler, rc::Rc,
    wrap_browser_process_handler, wrap_client, wrap_life_span_handler,
};

use crate::app::{GpuDeviceIds, SecurityConfig};

#[derive(Clone)]
pub struct OsrBrowserProcessHandler {
    is_cef_ready: RefCell<bool>,
    security_config: SecurityConfig,
    gpu_device_ids: Option<GpuDeviceIds>,
}

impl Default for OsrBrowserProcessHandler {
    fn default() -> Self {
        Self::new(SecurityConfig::default(), None)
    }
}

impl OsrBrowserProcessHandler {
    pub fn new(security_config: SecurityConfig, gpu_device_ids: Option<GpuDeviceIds>) -> Self {
        Self {
            is_cef_ready: RefCell::new(false),
            security_config,
            gpu_device_ids,
        }
    }
}

wrap_browser_process_handler! {
    pub(crate) struct BrowserProcessHandlerBuilder {
        handler: OsrBrowserProcessHandler,
    }

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            *self.handler.is_cef_ready.borrow_mut() = true;
        }

        /// The client for a window Chromium opens by itself -- an extension's welcome or
        /// "updated" tab, a chrome.tabs.create -- as opposed to the off-screen browsers this
        /// addon creates, which carry their own client. Those windows are real desktop windows
        /// a Godot app never asked for, so they are closed as soon as they exist. (Returning
        /// None instead would leave them open and unmanaged, and would block shutdown until
        /// someone closed them by hand.)
        fn default_client(&self) -> Option<Client> {
            Some(UnrequestedWindowClient::new(UnrequestedWindowCloser::new()))
        }

        fn on_before_child_process_launch(&self, command_line: Option<&mut CommandLine>) {
            let Some(command_line) = command_line else {
                return;
            };

            let security_config = &self.handler.security_config;
            if security_config.disable_web_security {
                command_line.append_switch(Some(&"disable-web-security".into()));
            }
            if security_config.allow_insecure_content {
                command_line.append_switch(Some(&"allow-running-insecure-content".into()));
            }
            if security_config.ignore_certificate_errors {
                command_line.append_switch(Some(&"ignore-certificate-errors".into()));
                command_line.append_switch(Some(&"ignore-ssl-errors".into()));
            }

            command_line.append_switch(Some(&"disable-session-crashed-bubble".into()));
            command_line.append_switch(Some(&"enable-logging=stderr".into()));

            if let Some(ids) = &self.handler.gpu_device_ids {
                command_line.append_switch_with_value(
                    Some(&"gpu-vendor-id".into()),
                    Some(&ids.to_vendor_arg().as_str().into()),
                );
                command_line.append_switch_with_value(
                    Some(&"gpu-device-id".into()),
                    Some(&ids.to_device_arg().as_str().into()),
                );
            }
        }
    }
}

impl BrowserProcessHandlerBuilder {
    pub(crate) fn build(handler: OsrBrowserProcessHandler) -> BrowserProcessHandler {
        Self::new(handler)
    }
}

wrap_life_span_handler! {
    pub(crate) struct UnrequestedWindowCloser {}

    impl LifeSpanHandler {
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            if let Some(host) = browser.and_then(|b| b.host()) {
                eprintln!("[godot_cef] closing a window Chromium opened by itself (e.g. an extension tab)");
                host.close_browser(1);
            }
        }
    }
}

wrap_client! {
    pub(crate) struct UnrequestedWindowClient {
        closer: LifeSpanHandler,
    }

    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.closer.clone())
        }
    }
}
