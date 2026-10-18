//! Minimal Simple Binary Encoding (SBE) decoder for Paradex public WS frames.
//!
//! Paradex made SBE **mandatory for public channel payloads** on 2026-09-21
//! (schema `paradex_1_0.xml`, schema id 1; prod serves version 1:2). The
//! JSON-RPC control plane stays text — only channel data arrives as binary
//! frames. This module decodes the one template the research plane consumes:
//!
//! - template 4 `MarketSummaryEvent` — 24h rolling stats + live quote of one
//!   market: mark / index / last, best bid & ask with sizes, venue IVs
//!   (mark/bid/ask/last), full greeks, open interest, 24h volume.
//!
//! Wire layout (all little-endian, verified live 2026-10-13 against the
//! official schema and a frame-by-frame Python cross-check):
//!
//! ```text
//! ┌──────────────────────── SBE message header (8 B) ───────────────────────┐
//! │ u16 blockLength │ u16 templateId │ u16 schemaId │ u16 version          │
//! ├───────────────────── root block: blockLength bytes ────────────────────┤
//! │ fixed fields, 8 bytes each, in schema declaration order                │
//! ├────────────────────── var-data section ────────────────────────────────┤
//! │ per <data> field: u8 length + UTF-8 bytes (varString8)                 │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! Versioning rules that shape this decoder (from the schema's release notes):
//! - Fields appended after 1:0 (`forwardRate`, `riskFreeRate`,
//!   `fundingRatePrecise`) sit at the END of the root block; a v0 frame has
//!   `blockLength == 216`, a v1+ frame `== 240`. **Never hardcode offsets** —
//!   read `blockLength` and treat fields beyond it as absent.
//! - Unknown template ids must be skipped, not rejected.
//! - Optional numerics use the i64::MIN sentinel; `mantissa × 10^-8`
//!   (Price8/Qty8/Value8/Rate8) or `× 10^-12` (Rate12).

/// SBE message header length in bytes.
pub const HEADER_LEN: usize = 8;
/// `MarketSummaryEvent` template id.
pub const TEMPLATE_MARKET_SUMMARY: u16 = 4;
/// `BookEvent` template id (order_book channel).
pub const TEMPLATE_BOOK: u16 = 3;
/// Root-block length of a BookEvent: ts(8) + seq(8) + pkgType(1) + 9 optional
/// 8-byte fields = 89 — "what every client has always received as version 0"
/// (schema's own history note; the interactive/API best-price fields predate
/// version marking, so no shorter layout was ever served).
pub const BOOK_BLOCK: usize = 89;
/// Root-block length of a version-0 MarketSummaryEvent (27 fields × 8 B).
pub const SUMMARY_BLOCK_V0: usize = 216;
/// Root-block length of a version-1+ MarketSummaryEvent (30 fields × 8 B).
pub const SUMMARY_BLOCK_V1: usize = 240;
/// i64 sentinel for "field absent" in optional SBE numerics.
pub const NULL_SENTINEL: i64 = i64::MIN;

/// Decoded `MarketSummaryEvent`. Prices/quantities are decimal (USD, coin);
/// rates are fractions (`mark_iv == 0.4975` → 49.75 vol points).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MarketSummary {
    /// Feed publish timestamp, ms since epoch.
    pub ts_ms: i64,
    pub mark: Option<f64>,
    /// Underlying index (oracle spot) the option settles against.
    pub index: Option<f64>,
    pub last: Option<f64>,
    /// 24h traded notional, USD.
    pub vol24h_usd: Option<f64>,
    /// Open interest, base-currency units.
    pub oi: Option<f64>,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub bid_size: Option<f64>,
    pub ask_size: Option<f64>,
    pub mark_iv: Option<f64>,
    pub bid_iv: Option<f64>,
    pub ask_iv: Option<f64>,
    pub last_iv: Option<f64>,
    pub delta: Option<f64>,
    pub gamma: Option<f64>,
    pub vega: Option<f64>,
    pub theta: Option<f64>,
}

impl MarketSummary {
    /// Effective price: prefer explicit bid/ask, then mark, then last trade.
    pub fn any_price(&self) -> Option<f64> {
        self.bid
            .or(self.ask)
            .or(self.mark)
            .or(self.last)
            .filter(|p| *p > 0.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SbeFrame {
    Summary(MarketSummary),
    /// A recognized-but-unconsumed template (bbo, trades, heartbeat, private…).
    Other(u16),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SbeError(pub &'static str);

impl std::fmt::Display for SbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sbe: {}", self.0)
    }
}
impl std::error::Error for SbeError {}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    let s = b.get(o..o + 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn i64_at(b: &[u8], o: usize) -> Option<i64> {
    let s = b.get(o..o + 8)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Some(i64::from_le_bytes(a))
}

/// mantissa × 10^-8, with the i64::MIN sentinel mapped to `None`. "Required"
/// and "optional" SBE fields decode identically here: both carry the
/// sentinel when the value is absent.
fn dec8(b: &[u8], o: usize) -> Option<Option<f64>> {
    Some(match i64_at(b, o)? {
        NULL_SENTINEL => None,
        m => Some(m as f64 * 1e-8),
    })
}

/// Read a `varString8` at `o`; returns `(bytes, next_offset)`.
fn var_str8(b: &[u8], o: usize) -> Option<(&[u8], usize)> {
    let n = *b.get(o)? as usize;
    let s = b.get(o + 1..o + 1 + n)?;
    Some((s, o + 1 + n))
}

/// Decode one binary WS frame into an [`SbeFrame`].
///
/// Malformed / truncated frames return `Err`; unknown templates return
/// `Ok(SbeFrame::Other(id))` so callers can skip them per the schema contract.
pub fn decode(b: &[u8]) -> Result<SbeFrame, SbeError> {
    let block_len = u16_at(b, 0).ok_or(SbeError("frame shorter than header"))? as usize;
    let template = u16_at(b, 2).ok_or(SbeError("frame shorter than header"))?;
    if template != TEMPLATE_MARKET_SUMMARY {
        return Ok(SbeFrame::Other(template));
    }
    let root = b
        .get(HEADER_LEN..HEADER_LEN + block_len)
        .ok_or(SbeError("root block truncated"))?;

    // Field offsets inside the root block: 8 bytes per field, in schema
    // declaration order (offset = 8 × declaration index). Fields beyond this
    // frame's negotiated blockLength are absent-by-version and read as None.
    let g = |i: usize| -> Option<f64> {
        if i * 8 + 8 <= root.len() {
            dec8(root, i * 8).flatten()
        } else {
            None
        }
    };
    let ts_ms = i64_at(root, 0).ok_or(SbeError("ts truncated"))? / 1000;

    Ok(SbeFrame::Summary(MarketSummary {
        ts_ms,
        mark: g(2),
        index: g(3),
        last: g(4),
        vol24h_usd: g(5),
        oi: g(6),
        bid: g(8),
        ask: g(9),
        bid_size: g(25),
        ask_size: g(26),
        mark_iv: g(12),
        bid_iv: g(13),
        ask_iv: g(14),
        last_iv: g(15),
        delta: g(16),
        gamma: g(17),
        vega: g(18),
        theta: g(19),
    }))
}

/// Decode a MarketSummaryEvent frame together with its market symbol (the
/// message's single var-data field).
pub fn decode_with_market(b: &[u8]) -> Result<(String, MarketSummary), SbeError> {
    let block_len = u16_at(b, 0).ok_or(SbeError("frame shorter than header"))? as usize;
    let template = u16_at(b, 2).ok_or(SbeError("frame shorter than header"))?;
    if template != TEMPLATE_MARKET_SUMMARY {
        return Err(SbeError("not a market summary frame"));
    }
    if b.len() < HEADER_LEN + block_len {
        return Err(SbeError("root block truncated"));
    }
    let (mkt, _) = var_str8(b, HEADER_LEN + block_len).ok_or(SbeError("market string truncated"))?;
    let market = std::str::from_utf8(mkt)
        .map_err(|_| SbeError("market not utf-8"))?
        .to_string();
    match decode(b)? {
        SbeFrame::Summary(s) => Ok((market, s)),
        _ => unreachable!("template checked above"),
    }
}

// ---------------------------------------------------------------------------
// BookEvent (template 3) — the order_book channel payload
// ---------------------------------------------------------------------------

/// One side of a book update: `(price, size)` levels, best level first
/// (the API snapshot feed sorts best-first — verified live against prod).
pub type BookLevels = Vec<(f64, f64)>;

/// Decoded `BookEvent` from the `order_book.{market}.{feed}@15@{rate}` channel.
///
/// The `@15@50ms` **snapshot** feed is what the tick plane subscribes to: a
/// complete ≤15-level picture of the API book (RPI excluded) on every change,
/// throttled by the exchange to at most one frame per 50 ms per market. The
/// BBO is `bids[0]` / `asks[0]`.
#[derive(Debug, Clone, PartialEq)]
pub struct BookUpdate {
    /// Feed publish timestamp, ms since epoch.
    pub ts_ms: i64,
    /// Per-market orderbook sequence (monotonic).
    pub seq: i64,
    /// True for a full snapshot frame, false for an incremental delta.
    pub is_snapshot: bool,
    /// Best bid price/size including RPI (interactive feeds only).
    pub best_bid: Option<(f64, f64)>,
    /// Best ask price/size including RPI (interactive feeds only).
    pub best_ask: Option<(f64, f64)>,
    /// Best bid price/size from API orders, excluding RPI, when published.
    pub best_bid_api: Option<(f64, f64)>,
    /// Best ask price/size from API orders, excluding RPI, when published.
    pub best_ask_api: Option<(f64, f64)>,
    pub bids: BookLevels,
    pub asks: BookLevels,
}

impl BookUpdate {
    /// Execution-grade BBO: the API book's first levels (RPI-excluded), which
    /// is what a routed order would actually touch.
    pub fn bbo(&self) -> (Option<(f64, f64)>, Option<(f64, f64)>) {
        let b = self
            .best_bid_api
            .or_else(|| self.bids.first().copied().map(|(p, s)| (p, s)))
            .or(self.best_bid);
        let a = self
            .best_ask_api
            .or_else(|| self.asks.first().copied().map(|(p, s)| (p, s)))
            .or(self.best_ask);
        (b, a)
    }
}

/// Decode a BookEvent frame together with its market symbol.
///
/// Wire layout (verified byte-for-byte against live prod frames, 2026-10-17):
/// ```text
/// header: blockLength=89, templateId=3, schemaId=1, version=2
/// root:   ts i64 @0 · seq i64 @8 · pkgType u8 @16 (0=SNAPSHOT, 1=DELTA)
///         bestBidPrice @17 · bestBidSize @25 · bestAskPrice @33 · bestAskSize @41
///         relativeSpread @49 · bestBidApiPrice @57 · bestBidApiSize @65
///         bestAskApiPrice @73 · bestAskApiSize @81        (all Price8NULL)
/// groups: bids { u16 entryLen; u16 n; n × entryLen } then asks likewise
///         (entry: price i64 mantissa, size i64 mantissa, ×10^-8)
/// var:    market varString8
/// ```
pub fn decode_book(b: &[u8]) -> Result<(String, BookUpdate), SbeError> {
    let block_len = u16_at(b, 0).ok_or(SbeError("frame shorter than header"))? as usize;
    let template = u16_at(b, 2).ok_or(SbeError("frame shorter than header"))?;
    if template != TEMPLATE_BOOK {
        return Err(SbeError("not a book frame"));
    }
    if block_len < BOOK_BLOCK {
        return Err(SbeError("book root block too short"));
    }
    let root = b
        .get(HEADER_LEN..HEADER_LEN + BOOK_BLOCK)
        .ok_or(SbeError("book root block truncated"))?;
    let ts_ms = i64_at(root, 0).ok_or(SbeError("ts truncated"))? / 1000;
    let seq = i64_at(root, 8).ok_or(SbeError("seq truncated"))?;
    let pkg = *root.get(16).ok_or(SbeError("pkgType truncated"))?;
    if pkg > 1 {
        return Err(SbeError("unknown book pkgType"));
    }
    let pair = |po: usize, so: usize| -> Option<(f64, f64)> {
        let p = dec8(root, po).flatten()?;
        let s = dec8(root, so).flatten()?;
        // A zero price is not a quote (live frames use the i64::MIN sentinel
        // for absent, but a zeroed buffer must not fabricate a 0.0 book).
        if p <= 0.0 {
            return None;
        }
        Some((p, s))
    };
    let mut o = HEADER_LEN + block_len;
    let levels = |o: &mut usize| -> Result<BookLevels, SbeError> {
        let (entry_len, n) = {
            let g = b.get(*o..*o + 4).ok_or(SbeError("group header truncated"))?;
            (
                u16::from_le_bytes([g[0], g[1]]) as usize,
                u16::from_le_bytes([g[2], g[3]]) as usize,
            )
        };
        *o += 4;
        // Hard cap guards against a corrupt count running off the frame.
        let n = n.min(64) * entry_len.max(1);
        let bytes = b.get(*o..*o + n).ok_or(SbeError("group entries truncated"))?;
        let mut lv = Vec::new();
        if entry_len >= 16 {
            for i in 0..(n / entry_len) {
                let e = &bytes[i * entry_len..i * entry_len + entry_len];
                let pm = i64_at(e, 0).ok_or(SbeError("level price truncated"))?;
                let sm = i64_at(e, 8).ok_or(SbeError("level size truncated"))?;
                if pm == NULL_SENTINEL {
                    continue;
                }
                lv.push((pm as f64 * 1e-8, sm as f64 * 1e-8));
            }
        }
        *o += n;
        Ok(lv)
    };
    let bids = levels(&mut o)?;
    let asks = levels(&mut o)?;
    let (mkt, _) = var_str8(b, o).ok_or(SbeError("market string truncated"))?;
    let market = std::str::from_utf8(mkt)
        .map_err(|_| SbeError("market not utf-8"))?
        .to_string();
    Ok((
        market,
        BookUpdate {
            ts_ms,
            seq,
            is_snapshot: pkg == 0,
            best_bid: pair(17, 25),
            best_ask: pair(33, 41),
            best_bid_api: pair(57, 65),
            best_ask_api: pair(73, 81),
            bids,
            asks,
        },
    ))
}

// ---------------------------------------------------------------------------
// Tests — every test builds frames byte-by-byte exactly as the schema
// defines, then asserts the decoder reads them back. The layout numbers were
// cross-checked against live prod frames (see scripts/probe_paradex_sbe.py).
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    fn frame(block: &[u8], market: &str) -> Vec<u8> {
        let mut b = Vec::with_capacity(HEADER_LEN + block.len() + 1 + market.len());
        b.extend_from_slice(&(block.len() as u16).to_le_bytes());
        b.extend_from_slice(&TEMPLATE_MARKET_SUMMARY.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes()); // schemaId
        b.extend_from_slice(&2u16.to_le_bytes()); // version 1:2
        b.extend_from_slice(block);
        b.push(market.len() as u8);
        b.extend_from_slice(market.as_bytes());
        b
    }

    fn put(root: &mut Vec<u8>, v: i64) {
        root.extend_from_slice(&v.to_le_bytes());
    }

    fn m(v: f64) -> i64 {
        (v * 1e8).round() as i64
    }

    /// mantissa→decimal round-trips through binary floats; compare with a
    /// 1e-9 relative epsilon instead of exact equality.
    fn feq(a: Option<f64>, b: f64) -> bool {
        match a {
            Some(x) => (x - b).abs() < 1e-9 * b.abs().max(1.0),
            None => false,
        }
    }

    #[test]
    fn decode_v1_full_summary() {
        let mut root = Vec::with_capacity(SUMMARY_BLOCK_V1);
        put(&mut root, 1_791_863_038_156_000); // ts (µs)
        put(&mut root, 7); // seq
        put(&mut root, m(8.49331527)); // mark
        put(&mut root, m(162.20985993)); // index
        put(&mut root, m(0.0)); // last
        put(&mut root, m(12_345.5)); // vol24h
        put(&mut root, m(42.0)); // oi
        put(&mut root, m(0.0001)); // funding
        put(&mut root, m(8.40)); // bid
        put(&mut root, m(8.60)); // ask
        put(&mut root, m(9_000_000.0)); // totalVolume
        put(&mut root, m(0.255649)); // chg24h
        put(&mut root, m(0.49750658)); // markIv
        put(&mut root, NULL_SENTINEL); // bidIv
        put(&mut root, m(0.53)); // askIv
        put(&mut root, NULL_SENTINEL); // lastIv
        put(&mut root, m(-0.81732522)); // delta
        put(&mut root, m(0.03275929)); // gamma
        put(&mut root, m(0.04320987)); // vega
        put(&mut root, m(-0.27290679)); // theta
        put(&mut root, m(-0.0008558)); // rho
        put(&mut root, m(0.00074828)); // volga
        put(&mut root, m(-0.00051053)); // vanna
        put(&mut root, m(0.00010039)); // futureFundingRate
        put(&mut root, NULL_SENTINEL); // externalFairPrice
        put(&mut root, m(1.25)); // bidSize
        put(&mut root, m(0.75)); // askSize
        put(&mut root, m(0.05327093)); // forwardRate (v1)
        put(&mut root, m(0.0)); // riskFreeRate (v1)
        put(&mut root, NULL_SENTINEL); // fundingRatePrecise (v1)
        assert_eq!(root.len(), SUMMARY_BLOCK_V1);

        let bytes = frame(&root, "SPCX-USD-16OCT26-170-P");
        let (mkt, s) = decode_with_market(&bytes).unwrap();
        assert_eq!(mkt, "SPCX-USD-16OCT26-170-P");
        assert_eq!(s.ts_ms, 1_791_863_038_156);
        assert!(feq(s.mark, 8.49331527));
        assert!(feq(s.index, 162.20985993));
        assert!(feq(s.vol24h_usd, 12_345.5));
        assert!(feq(s.oi, 42.0));
        assert!(feq(s.bid, 8.40));
        assert!(feq(s.ask, 8.60));
        assert!(feq(s.bid_size, 1.25));
        assert!(feq(s.ask_size, 0.75));
        assert!(feq(s.mark_iv, 0.49750658));
        assert_eq!(s.bid_iv, None);
        assert!(feq(s.ask_iv, 0.53));
        assert!(feq(s.delta, -0.81732522));
        assert!(feq(s.theta, -0.27290679));
        assert!(feq(s.any_price(), 8.40));
    }

    #[test]
    fn decode_v0_short_block_stops_cleanly() {
        // A v0 frame (216-byte root) must parse without the three appended fields.
        let mut root = Vec::with_capacity(SUMMARY_BLOCK_V0);
        put(&mut root, 1_791_863_038_156_000);
        put(&mut root, 0);
        for _ in 0..25 {
            put(&mut root, NULL_SENTINEL);
        }
        assert_eq!(root.len(), SUMMARY_BLOCK_V0);
        let bytes = frame(&root, "BTC-USD-16OCT26-84000-C");
        let (mkt, s) = decode_with_market(&bytes).unwrap();
        assert_eq!(mkt, "BTC-USD-16OCT26-84000-C");
        assert_eq!(s.ts_ms, 1_791_863_038_156);
        assert_eq!(s.mark, None);
        assert_eq!(s.mark_iv, None);
        assert_eq!(s.any_price(), None);
    }

    #[test]
    fn unknown_templates_are_skipped_not_errors() {
        // bbo template 2 with a plausible body — must decode as Other(2).
        let mut b = Vec::new();
        b.extend_from_slice(&48u16.to_le_bytes()); // blockLength
        b.extend_from_slice(&2u16.to_le_bytes()); // templateId
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend(std::iter::repeat_n(0u8, 48));
        b.push(3);
        b.extend_from_slice(b"BTC");
        assert_eq!(decode(&b).unwrap(), SbeFrame::Other(2));
    }

    #[test]
    fn truncated_frames_error() {
        assert!(decode(&[]).is_err());
        assert!(decode(&[0, 0, 4, 0, 1, 0]).is_err()); // header cut
        // block claims 240 bytes, only 40 present
        let mut b = vec![240u16.to_le_bytes()[0], 240u16.to_le_bytes()[1], 4, 0, 1, 0, 2, 0];
        b.extend(std::iter::repeat_n(0u8, 40));
        assert!(decode(&b).is_err());
    }

    #[test]
    fn negative_and_tiny_mantissas() {
        let mut root = Vec::with_capacity(SUMMARY_BLOCK_V0);
        put(&mut root, 1_000_000_000i64 * 1_000_000); // 1e15 µs
        put(&mut root, 0);
        put(&mut root, m(0.00000001)); // mark: smallest positive at -8
        for _ in 0..24 {
            put(&mut root, NULL_SENTINEL);
        }
        let bytes = frame(&root, "X-USD-01JAN27-1-P");
        let (_, s) = decode_with_market(&bytes).unwrap();
        assert!(feq(s.mark, 1e-8));
    }

    // ---- BookEvent (template 3) -------------------------------------------

    /// Build a BookEvent frame exactly as the schema defines.
    fn book_frame(
        root_extras: &[(usize, f64)],
        bids: &[(f64, f64)],
        asks: &[(f64, f64)],
        market: &str,
        pkg: u8,
    ) -> Vec<u8> {
        let mut root = vec![0u8; BOOK_BLOCK];
        root[0..8].copy_from_slice(&1_792_319_576_852_000i64.to_le_bytes()); // ts µs
        root[8..16].copy_from_slice(&7_695_297_293i64.to_le_bytes()); // seq
        root[16] = pkg;
        for (off, v) in root_extras {
            root[*off..*off + 8].copy_from_slice(&m(*v).to_le_bytes());
        }
        let mut b = Vec::new();
        b.extend_from_slice(&(BOOK_BLOCK as u16).to_le_bytes());
        b.extend_from_slice(&TEMPLATE_BOOK.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes()); // schemaId
        b.extend_from_slice(&2u16.to_le_bytes()); // version
        b.extend_from_slice(&root);
        for side in [bids, asks] {
            b.extend_from_slice(&16u16.to_le_bytes()); // entry blockLength
            b.extend_from_slice(&(side.len() as u16).to_le_bytes());
            for (p, s) in side {
                b.extend_from_slice(&m(*p).to_le_bytes());
                b.extend_from_slice(&m(*s).to_le_bytes());
            }
        }
        b.push(market.len() as u8);
        b.extend_from_slice(market.as_bytes());
        b
    }

    #[test]
    fn decode_book_snapshot_with_levels() {
        // Mirrors a live BTC-USD-PERP snapshot@15@50ms frame: all nine
        // optional best-price fields NULL (non-interactive feed), levels only.
        let bids = [(82154.9, 0.01217), (82150.6, 0.0003), (82144.3, 0.02315)];
        let asks = [(82181.2, 0.02315), (82199.5, 0.00038)];
        let bytes = book_frame(&[], &bids, &asks, "BTC-USD-PERP", 0);
        let (mkt, bu) = decode_book(&bytes).unwrap();
        assert_eq!(mkt, "BTC-USD-PERP");
        assert!(bu.is_snapshot);
        assert_eq!(bu.ts_ms, 1_792_319_576_852);
        assert_eq!(bu.seq, 7_695_297_293);
        assert_eq!(bu.best_bid, None);
        assert_eq!(bu.best_bid_api, None);
        assert_eq!(bu.bids.len(), 3);
        assert!(feq(Some(bu.bids[0].0), 82154.9));
        assert!(feq(Some(bu.bids[0].1), 0.01217));
        assert_eq!(bu.asks.len(), 2);
        let (b, a) = bu.bbo();
        let (b, a) = (b.unwrap(), a.unwrap());
        assert!(feq(Some(b.0), 82154.9));
        assert!(feq(Some(a.0), 82181.2));
    }

    #[test]
    fn decode_book_interactive_best_fields() {
        // An interactive feed populates the root best-price pairs; bbo()
        // prefers the API pair, then levels, then interactive.
        let extras = [(17, 100.5), (25, 3.0), (33, 101.5), (41, 4.0), (57, 100.0), (65, 1.0), (73, 102.0), (81, 2.0)];
        let bytes = book_frame(&extras, &[(100.0, 1.0)], &[(102.0, 2.0)], "ETH-USD-PERP", 1);
        let (_, bu) = decode_book(&bytes).unwrap();
        assert!(!bu.is_snapshot);
        assert_eq!(bu.best_bid_api, Some((100.0, 1.0)));
        assert_eq!(bu.best_ask_api, Some((102.0, 2.0)));
        let (b, a) = bu.bbo();
        assert_eq!(b, Some((100.0, 1.0)));
        assert_eq!(a, Some((102.0, 2.0)));
    }

    #[test]
    fn decode_book_truncated_and_wrong_template() {
        let good = book_frame(&[], &[(1.0, 1.0)], &[(2.0, 1.0)], "X", 0);
        assert!(decode_book(&good[..good.len() - 1]).is_err()); // market cut
        let mut short = good.clone();
        short[0..2].copy_from_slice(&40u16.to_le_bytes()); // blockLength < 89
        assert!(decode_book(&short).is_err());
        let mut wrong = good;
        wrong[2..4].copy_from_slice(&TEMPLATE_MARKET_SUMMARY.to_le_bytes());
        assert!(decode_book(&wrong).is_err());
        // group count claiming more entries than present must error, not panic
        let mut ghost = book_frame(&[], &[(1.0, 1.0)], &[(2.0, 1.0)], "BTC-USD-PERP", 0);
        let bids_n_off = HEADER_LEN + BOOK_BLOCK + 2;
        ghost[bids_n_off..bids_n_off + 2].copy_from_slice(&500u16.to_le_bytes());
        assert!(decode_book(&ghost).is_err());
    }
}
