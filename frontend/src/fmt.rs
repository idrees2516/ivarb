//! Display formatting helpers (vol points, money, ages, strikes).

/// Insert thousands separators into an integer string.
fn group(n: i64) -> String {
    let neg = n < 0;
    let s = n.unsigned_abs().to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3 + 2);
    if neg {
        out.push('-');
    }
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

pub fn fmt_vp(v: f64) -> String {
    format!("{v:+.2}")
}

pub fn fmt_vp_plain(v: f64) -> String {
    format!("{v:.2}")
}

pub fn fmt_usd(v: f64) -> String {
    if !v.is_finite() {
        return "—".into();
    }
    let a = v.abs();
    if a >= 100.0 {
        format!("${}", group(v.round() as i64))
    } else if a >= 1.0 {
        format!("${v:.1}")
    } else {
        format!("${v:.2}")
    }
}

pub fn fmt_int(v: f64) -> String {
    group(v.round() as i64)
}

pub fn fmt_spot(v: f64) -> String {
    format!("${}", group(v.round() as i64))
}

pub fn fmt_strike(k: f64) -> String {
    if k >= 1000.0 {
        group(k.round() as i64)
    } else {
        format!("{k:.1}")
    }
}

pub fn fmt_age(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    }
}

pub fn fmt_short_time(ms: i64) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms as f64));
    let h = d.get_utc_hours();
    let m = d.get_utc_minutes();
    format!("{h:02}:{m:02}")
}

/// Venue display name: strip the `sim:` prefix (the SIM chip shows it instead).
pub fn venue_name(v: &str) -> String {
    v.strip_prefix("sim:").unwrap_or(v).to_string()
}

pub fn fmt_right(r: &ivarb_protocol::Right) -> &'static str {
    match r {
        ivarb_protocol::Right::Call => "CALL",
        ivarb_protocol::Right::Put => "PUT",
    }
}

pub fn fmt_conn_state(s: &ivarb_protocol::ConnState) -> &'static str {
    use ivarb_protocol::ConnState::*;
    match s {
        Connecting => "connecting",
        Live => "live",
        Degraded => "degraded",
        Disconnected => "disconnected",
        Disabled => "disabled",
    }
}
