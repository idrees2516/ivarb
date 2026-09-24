//! Defensive JSON field access: venue payloads vary in casing, quoting and
//! optional fields, so every parser goes through these tolerant getters.

use serde_json::Value;

/// Fetch the first present field (by any of `names`) as f64.
/// Accepts JSON numbers, numeric strings and empty strings (→ None).
pub fn jf(v: &Value, names: &[&str]) -> Option<f64> {
    for n in names {
        if let Some(x) = v.get(*n) {
            match x {
                Value::Number(num) => {
                    if let Some(f) = num.as_f64() {
                        if f.is_finite() {
                            return Some(f);
                        }
                    }
                }
                Value::String(s) => {
                    let t = s.trim();
                    if t.is_empty() || t.eq_ignore_ascii_case("null") {
                        continue;
                    }
                    if let Ok(f) = t.parse::<f64>() {
                        if f.is_finite() {
                            return Some(f);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Fetch the first present field as i64 (numbers, numeric strings).
pub fn ji(v: &Value, names: &[&str]) -> Option<i64> {
    jf(v, names).map(|f| f as i64)
}

/// Fetch the first present field as a trimmed String.
pub fn js(v: &Value, names: &[&str]) -> Option<String> {
    for n in names {
        if let Some(x) = v.get(*n) {
            match x {
                Value::String(s) => {
                    let t = s.trim();
                    if !t.is_empty() {
                        return Some(t.to_string());
                    }
                }
                Value::Number(num) => return Some(num.to_string()),
                _ => {}
            }
        }
    }
    None
}

/// Normalize an IV value that may be quoted either in vol points (62.3) or as
/// a raw fraction (0.623). Crypto IVs live in 10..400 vp, so values below 3.0
/// are treated as fractions and scaled ×100.
pub fn norm_iv_vp(v: f64) -> Option<f64> {
    if !v.is_finite() || v <= 0.0 {
        return None;
    }
    let vp = if v < 3.0 { v * 100.0 } else { v };
    if (1.0..=500.0).contains(&vp) {
        Some(vp)
    } else {
        None
    }
}
