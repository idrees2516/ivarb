//! Global reactive state and the WebSocket feed client.

use std::cell::RefCell;
use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use ivarb_protocol::{
    EngineStats, ExecCommand, ExecSnapshot, GaCommand, GaStatus, HistoryPt, HoldCandidate,
    MultiHopRoute, Opportunity, ServerMsg, SpotQuote, VenueStatus,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Newest,
    NetVp,
    EstUsd,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UndFilter {
    All,
    Btc,
    Eth,
    Sol,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WsState {
    Connecting,
    Live,
    Reconnecting,
}

fn now_ms() -> i64 {
    js_sys::Date::now() as i64
}

#[derive(Clone)]
pub struct ApiState {
    pub opps: RwSignal<Vec<Opportunity>>,
    pub histories: RwSignal<HashMap<String, Vec<HistoryPt>>>,
    pub venues: RwSignal<Vec<VenueStatus>>,
    pub stats: RwSignal<Option<EngineStats>>,
    pub spots: RwSignal<Vec<SpotQuote>>,
    pub exec: RwSignal<Option<ExecSnapshot>>,
    /// Hold-to-expiry lab candidates (near-miss divergences below the gate).
    pub holds: RwSignal<Vec<HoldCandidate>>,
    /// Live multi-hop arbitrage routes.
    pub multihop: RwSignal<Vec<MultiHopRoute>>,
    /// Genetic-optimizer status.
    pub ga: RwSignal<Option<GaStatus>>,
    pub selected: RwSignal<Option<String>>,
    /// Selected hold-lab candidate (inst id).
    pub selected_hold: RwSignal<Option<String>>,
    pub sort: RwSignal<SortMode>,
    pub und_filter: RwSignal<UndFilter>,
    pub ws_state: RwSignal<WsState>,
    pub now: RwSignal<i64>,
    /// Flash message after a taker command ("armed", "rejected"…).
    pub flash: RwSignal<Option<String>>,
}

thread_local! {
    static CURRENT: RefCell<Option<ApiState>> = const { RefCell::new(None) };
    static WS_SLOT: RefCell<Option<web_sys::WebSocket>> = const { RefCell::new(None) };
    static ATTEMPTS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn ws_url() -> String {
    let loc = web_sys::window().expect("window").location();
    let proto = if loc.protocol().unwrap_or_default() == "https:" {
        "wss"
    } else {
        "ws"
    };
    let host = loc.host().unwrap_or_else(|_| "localhost:3000".into());
    format!("{proto}://{host}/ws")
}

/// Schedule a one-shot callback (ms).
pub fn set_timeout(ms: i32, f: impl FnOnce() + 'static) {
    let cb = Closure::once(Box::new(f) as Box<dyn FnOnce()>);
    let _ = web_sys::window()
        .unwrap()
        .set_timeout_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), ms);
    cb.forget();
}

impl ApiState {
    pub fn start() -> Self {
        let st = Self {
            opps: RwSignal::new(Vec::new()),
            histories: RwSignal::new(HashMap::new()),
            venues: RwSignal::new(Vec::new()),
            stats: RwSignal::new(None),
            spots: RwSignal::new(Vec::new()),
            exec: RwSignal::new(None),
            holds: RwSignal::new(Vec::new()),
            multihop: RwSignal::new(Vec::new()),
            ga: RwSignal::new(None),
            selected: RwSignal::new(None),
            selected_hold: RwSignal::new(None),
            sort: RwSignal::new(SortMode::Newest),
            und_filter: RwSignal::new(UndFilter::All),
            ws_state: RwSignal::new(WsState::Connecting),
            now: RwSignal::new(now_ms()),
            flash: RwSignal::new(None),
        };
        CURRENT.with(|c| *c.borrow_mut() = Some(st.clone()));
        st.connect_ws();
        st.start_ticker();
        st
    }

    /// POST a command to the advantage taker (fire-and-forget with a flash).
    pub fn send_exec_cmd(&self, cmd: ExecCommand, label: String) {
        let flash = self.flash;
        spawn_local(async move {
            match post_json("/api/exec/cmd", &cmd).await {
                true => flash.set(Some(label)),
                false => flash.set(Some(format!("{label} — executor unreachable"))),
            }
            set_timeout(4000, move || flash.set(None));
        });
    }

    /// POST a command to the genetic optimizer (fire-and-forget with a flash).
    pub fn send_ga_cmd(&self, cmd: GaCommand, label: String) {
        let flash = self.flash;
        spawn_local(async move {
            match post_json("/api/ga/cmd", &cmd).await {
                true => flash.set(Some(label)),
                false => flash.set(Some(format!("{label} — optimizer unreachable"))),
            }
            set_timeout(4000, move || flash.set(None));
        });
    }

    fn start_ticker(&self) {
        let set_now = self.now;
        let cb = Closure::wrap(Box::new(move || {
            set_now.set(now_ms());
        }) as Box<dyn FnMut()>);
        let _ = web_sys::window().unwrap().set_interval_with_callback_and_timeout_and_arguments_0(
            cb.as_ref().unchecked_ref(),
            1000,
        );
        cb.forget();
    }

    fn connect_ws(&self) {
        let (opps, set_opps) = (self.opps, self.opps.write_only());
        let (hist, set_hist) = (self.histories, self.histories.write_only());
        let (venues, set_venues) = (self.venues, self.venues.write_only());
        let (stats, set_stats) = (self.stats, self.stats.write_only());
        let (spots, set_spots) = (self.spots, self.spots.write_only());
        let (_holds, set_holds) = (self.holds, self.holds.write_only());
        let (_mh, set_multihop) = (self.multihop, self.multihop.write_only());
        let (_ga, set_ga) = (self.ga, self.ga.write_only());
        let (ws_state, set_ws_state) = (self.ws_state, self.ws_state.write_only());

        let ws = match web_sys::WebSocket::new(&ws_url()) {
            Ok(ws) => ws,
            Err(_) => {
                set_ws_state.set(WsState::Reconnecting);
                Self::schedule_reconnect();
                return;
            }
        };

        {
            let cb = Closure::once(Box::new(move || {
                ATTEMPTS.with(|a| a.set(0));
                set_ws_state.set(WsState::Live);
            }) as Box<dyn FnOnce()>);
            ws.set_onopen(Some(cb.as_ref().unchecked_ref()));
            cb.forget();
        }

        {
            let set_exec = self.exec.write_only();
            let cb = Closure::wrap(Box::new(move |ev: web_sys::MessageEvent| {
                let txt = match ev.data().as_string() {
                    Some(t) => t,
                    None => return,
                };
                let msg: ServerMsg = match serde_json::from_str(&txt) {
                    Ok(m) => m,
                    Err(e) => {
                        crate::console_err(&format!("ws parse error: {e}"));
                        return;
                    }
                };
                match msg {
                    ServerMsg::Snapshot { snapshot } => {
                        set_stats.set(Some(snapshot.stats));
                        set_venues.set(snapshot.venues);
                        set_spots.set(snapshot.spots);
                        set_holds.set(snapshot.holds);
                        set_multihop.set(snapshot.multihop);
                        let mut hm: HashMap<String, Vec<HistoryPt>> = HashMap::new();
                        for o in &snapshot.opportunities {
                            if let Some(h) = &o.history {
                                hm.insert(o.id.clone(), h.clone());
                            }
                        }
                        set_hist.set(hm);
                        set_opps.set(snapshot.opportunities);
                    }
                    ServerMsg::Opps { items } => {
                        // Merge deltas by id; `retired` removes.
                        let mut pts: Vec<(String, HistoryPt)> = Vec::new();
                        opps.update(|cur| {
                            for item in items {
                                if item.retired {
                                    cur.retain(|o| o.id != item.id);
                                    continue;
                                }
                                if let Some(pt) = item.hist_pt {
                                    pts.push((item.id.clone(), pt));
                                }
                                if let Some(slot) = cur.iter_mut().find(|o| o.id == item.id) {
                                    let pt = item.hist_pt;
                                    *slot = item;
                                    slot.hist_pt = pt;
                                } else {
                                    cur.push(item);
                                }
                            }
                        });
                        hist.update(|hm| {
                            for (id, pt) in pts {
                                let v = hm.entry(id).or_default();
                                match v.last_mut() {
                                    Some(last) if last.0 == pt.0 => last.1 = pt.1,
                                    Some(_) => {
                                        v.push(pt);
                                        if v.len() > 240 {
                                            v.remove(0);
                                        }
                                    }
                                    None => v.push(pt),
                                }
                            }
                        });
                    }
                    ServerMsg::Venues { items } => set_venues.set(items),
                    ServerMsg::Stats { stats } => set_stats.set(Some(stats)),
                    ServerMsg::Spots { items } => set_spots.set(items),
                    ServerMsg::Exec { exec } => set_exec.set(Some(exec)),
                    ServerMsg::Holds { items } => set_holds.set(items),
                    ServerMsg::Multihop { items } => set_multihop.set(items),
                    ServerMsg::Ga { status } => set_ga.set(Some(status)),
                }
            }) as Box<dyn FnMut(web_sys::MessageEvent)>);
            ws.set_onmessage(Some(cb.as_ref().unchecked_ref()));
            cb.forget();
        }

        {
            let cb = Closure::once(Box::new(move || {
                set_ws_state.set(WsState::Reconnecting);
                WS_SLOT.with(|s| *s.borrow_mut() = None);
                Self::schedule_reconnect();
            }) as Box<dyn FnOnce()>);
            ws.set_onclose(Some(cb.as_ref().unchecked_ref()));
            cb.forget();
        }
        {
            let cb = Closure::once(Box::new(move || {
                // onerror is usually followed by onclose; only flag here.
                set_ws_state.set(WsState::Reconnecting);
            }) as Box<dyn FnOnce()>);
            ws.set_onerror(Some(cb.as_ref().unchecked_ref()));
            cb.forget();
        }

        // Hold a reference so the socket is not garbage collected.
        WS_SLOT.with(|s| *s.borrow_mut() = Some(ws));
    }

    fn schedule_reconnect() {
        let delay = ATTEMPTS.with(|a| {
            let n = a.get();
            a.set(n + 1);
            (1000u32 * 2u32.saturating_pow(n.min(4))).min(15_000)
        });
        set_timeout(delay as i32, || {
            CURRENT.with(|c| {
                if let Some(st) = c.borrow().as_ref() {
                    st.ws_state.set(WsState::Connecting);
                    st.connect_ws();
                }
            });
        });
    }
}

/// POST JSON to the server; returns success.
async fn post_json<T: serde::Serialize>(url: &str, body: &T) -> bool {
    let Ok(body) = serde_json::to_string(body) else { return false };
    let opts = web_sys::RequestInit::new();
    opts.set_method("POST");
    opts.set_body(&wasm_bindgen::JsValue::from_str(&body));
    let headers = web_sys::Headers::new().ok();
    if let Some(h) = &headers {
        let _ = h.set("Content-Type", "application/json");
        opts.set_headers(h);
    }
    let Ok(req) = web_sys::Request::new_with_str_and_init(url, &opts) else { return false };
    let Some(win) = web_sys::window() else { return false };
    let resp = wasm_bindgen_futures::JsFuture::from(win.fetch_with_request(&req)).await;
    match resp {
        Ok(v) => web_sys::Response::from(v).ok(),
        Err(_) => false,
    }
}
