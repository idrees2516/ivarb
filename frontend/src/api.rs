//! REST fetch helpers for the research plane (and any GET/POST that needs
//! a deserialized response, unlike the fire-and-forget helpers in state.rs).

use leptos::task::spawn_local;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

/// GET JSON and deserialize. `None` on any transport/parse failure.
pub async fn get_json<T: serde::de::DeserializeOwned>(url: &str) -> Option<T> {
    let Some(win) = web_sys::window() else { return None };
    let resp = wasm_bindgen_futures::JsFuture::from(win.fetch_with_str(url)).await;
    let v = match resp {
        Ok(v) => web_sys::Response::from(v),
        Err(_) => return None,
    };
    if !v.ok() {
        return None;
    }
    let txt = wasm_bindgen_futures::JsFuture::from(v.text().ok()?).await;
    let txt = match txt {
        Ok(t) => t.as_string()?,
        Err(_) => return None,
    };
    serde_json::from_str(&txt).ok()
}

/// POST JSON and deserialize the response. `None` on failure.
pub async fn post_json_resp<T: serde::de::DeserializeOwned, B: serde::Serialize>(
    url: &str,
    body: &B,
) -> Option<T> {
    let Ok(body) = serde_json::to_string(body) else { return None };
    let opts = web_sys::RequestInit::new();
    opts.set_method("POST");
    opts.set_body(&wasm_bindgen::JsValue::from_str(&body));
    let headers = web_sys::Headers::new().ok();
    if let Some(h) = &headers {
        let _ = h.set("Content-Type", "application/json");
        opts.set_headers(h);
    }
    let Ok(req) = web_sys::Request::new_with_str_and_init(url, &opts) else { return None };
    let Some(win) = web_sys::window() else { return None };
    let resp = wasm_bindgen_futures::JsFuture::from(win.fetch_with_request(&req)).await;
    let v = match resp {
        Ok(v) => web_sys::Response::from(v),
        Err(_) => return None,
    };
    if !v.ok() {
        return None;
    }
    let txt = wasm_bindgen_futures::JsFuture::from(v.text().ok()?).await;
    let txt = match txt {
        Ok(t) => t.as_string()?,
        Err(_) => return None,
    };
    serde_json::from_str(&txt).ok()
}

/// Schedule a repeating callback (ms). The closure is leaked (page-lifetime),
/// matching the existing ticker pattern in state.rs.
pub fn set_interval(ms: i32, f: impl Fn() + 'static) {
    let cb = Closure::wrap(Box::new(f) as Box<dyn FnMut()>);
    let _ = web_sys::window()
        .unwrap()
        .set_interval_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), ms);
    cb.forget();
}

/// Fire-and-forget async task on the WASM runtime.
pub fn spawn(f: impl std::future::Future<Output = ()> + 'static) {
    spawn_local(f);
}
