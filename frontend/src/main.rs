use leptos::prelude::*;
use wasm_bindgen::prelude::*;

mod app;
mod chart;
mod fmt;
mod sections;
mod sidepanel;
mod state;

fn main() {
    console_log("ivarb frontend booting");
    mount_to_body(|| {
        view! { <app::App/> }
    });
}

pub fn console_log(s: &str) {
    web_sys::console::log_1(&JsValue::from_str(s));
}

pub fn console_err(s: &str) {
    web_sys::console::error_1(&JsValue::from_str(s));
}
