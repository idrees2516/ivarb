//! Realistic simulation feed: keeps the scanner fully demonstrable when live
//! venues are unreachable (or when `IVARB_SIM=always`).
//!
//! Generates correlated IV surfaces per venue with term structure, skew, OU
//! instrument noise and expiry-level divergence events that create genuine
//! fee-adjusted arb windows — the same economics the strategy layer must
//! handle on real data.

use std::time::Duration;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use ivarb_protocol::{ConnState, SpotQuote};
use ivarb_quant::bs_price;

use crate::key::{cap_expiries, InstKey, RIGHT_CALL, RIGHT_PUT};
use crate::msg::{now_ms, InstrTick, VenueMsg};
use crate::venues::{rand_range, status, VenueConnector, VenueCtx};

const YEAR_MS: f64 = 365.0 * 24.0 * 3600.0 * 1000.0;

fn normal(rng: &mut StdRng) -> f64 {
    let u1: f64 = rng.gen_range(1e-9..1.0);
    let u2: f64 = rng.gen_range(0.0..1.0);
    (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

fn anchor_for(und: &str) -> f64 {
    match und {
        "BTC" => 84_383.0,
        "ETH" => 3_210.0,
        "SOL" => 190.0,
        _ => 100.0,
    }
}

struct SimInst {
    key: InstKey,
    m: f64,         // ln(K / anchor)
    noise_vp: f64,  // OU state
    hl_ms: f64,     // noise half-life
    vov_vp: f64,    // noise vol per tick
    hs_vp: f64,     // quote half-spread (vp)
    size_bid: f64,
    size_ask: f64,
}

struct SimExpiry {
    expiry_ms: i64,
    base_vp: f64,
    venue_off_vp: f64,
    drift_vp: f64,
    drift_tau_ms: f64,
    insts: Vec<SimInst>,
}

struct SimUnd {
    name: String,
    anchor: f64,
    ou: f64,
    expiries: Vec<SimExpiry>,
}

pub struct SimVenue {
    label: String,
    seed: u64,
}

impl SimVenue {
    pub fn new(label: String) -> Self {
        let seed = label.bytes().map(|b| b as u64).fold(0x9E3779B97F4A7C15, |a, b| {
            a ^ (b.wrapping_mul(0x100000001B3))
        });
        Self { label, seed }
    }
}

#[async_trait::async_trait]
impl VenueConnector for SimVenue {
    fn id(&self) -> &str {
        &self.label
    }

    async fn run(&mut self, mut ctx: VenueCtx) -> anyhow::Result<()> {
        let venue_id = format!("sim:{}", self.label);
        let mut rng = StdRng::seed_from_u64(self.seed);

        // Register the sim venue with the engine.
        let mut spec = ctx.spec.clone();
        spec.id = venue_id.clone();
        let _ = ctx.tx.send(VenueMsg::Register { spec, is_sim: true }).await;
        status(&ctx.tx, &venue_id, ConnState::Connecting, None).await;

        let tick_ms: u64 = 500;
        let mut unds: Vec<SimUnd> = Vec::new();
        {
            let names: Vec<String> = ctx.cfg.underlyings.clone();
            for name in names {
                let und_id = ctx.intern(&name);
                let anchor = anchor_for(&name);
                let now = now_ms();
                let mut dates: Vec<u32> = [1i64, 2, 3, 4, 37, 93, 185, 275]
                    .iter()
                    .filter_map(|off| {
                        let ms = now + off * 86_400_000;
                        date_from_ms(ms)
                    })
                    .collect();
                cap_expiries(
                    &mut dates,
                    now,
                    ctx.cfg.min_dte,
                    ctx.cfg.max_dte,
                    ctx.cfg.max_expiries,
                );
                let mut expiries = Vec::new();
                for &date in &dates {
                    let expiry_ms = crate::key::ymd_to_expiry_ms(date).unwrap_or(now + 86_400_000);
                    let dte = ((expiry_ms - now) as f64 / 86_400_000.0).max(1.0);
                    let base_vp = 48.0 + 14.0 * (-dte / 45.0).exp();
                    let venue_off_vp = normal(&mut rng) * 0.25;
                    let mut insts = Vec::new();
                    for i in -12i32..=12 {
                        let k = anchor * (i as f64 * 0.025).exp();
                        let tick = 10f64.powi((anchor.log10().round() as i32) - 3);
                        let k = (k / tick).round() * tick;
                        if k <= 0.0 {
                            continue;
                        }
                        let m = (k / anchor).ln();
                        for &right in &[RIGHT_CALL, RIGHT_PUT] {
                            insts.push(SimInst {
                                key: InstKey {
                                    und: und_id,
                                    date,
                                    strike_ticks: (k * 10_000.0).round() as u64,
                                    right,
                                },
                                m,
                                noise_vp: normal(&mut rng) * 0.15,
                                hl_ms: rng.gen_range(300_000.0..900_000.0),
                                vov_vp: rng.gen_range(0.18..0.35),
                                hs_vp: 0.08 + 0.10 * m.abs() + 0.05 * (dte.min(90.0) / 90.0),
                                size_bid: 5.0 + rng.gen::<f64>().powi(2) * 250.0,
                                size_ask: 5.0 + rng.gen::<f64>().powi(2) * 250.0,
                            });
                        }
                    }
                    expiries.push(SimExpiry {
                        expiry_ms,
                        base_vp,
                        venue_off_vp,
                        drift_vp: 0.0,
                        drift_tau_ms: 200_000.0,
                        insts,
                    });
                }
                unds.push(SimUnd {
                    name,
                    anchor,
                    ou: 0.0,
                    expiries,
                });
            }
        }

        let total: u32 = unds
            .iter()
            .map(|u| u.expiries.iter().map(|e| e.insts.len() as u32).sum::<u32>())
            .sum();
        let _ = ctx
            .tx
            .send(VenueMsg::Tracked {
                venue: venue_id.clone(),
                instruments: total,
            })
            .await;
        status(&ctx.tx, &venue_id, ConnState::Live, Some("simulated feed".into())).await;

        let dt_s = tick_ms as f64 / 1000.0;
        loop {
            if ctx.shutdown() {
                return Ok(());
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(tick_ms)) => {}
                _ = ctx.shutdown.changed() => {
                    if ctx.shutdown() { return Ok(()); }
                }
            }

            let now = now_ms();
            for u in &mut unds {
                // Spot: log-OU around the anchor. Vol scaled so the
                // stationary cross-venue dispersion is ~0.8 % — wide enough
                // to create tradeable premium dislocations, sane enough that
                // box-style routes bank believable money instead of fantasy
                // millions off 10 % spot gaps.
                u.ou += -u.ou * (dt_s / 180.0) + 0.0009 * dt_s.sqrt() * normal(&mut rng);
                let spot = u.anchor * u.ou.exp();

                let _ = ctx
                    .tx
                    .send(VenueMsg::Spot(SpotQuote {
                        underlying: u.name.clone(),
                        venue: venue_id.clone(),
                        price: spot,
                        ts_ms: now,
                    }))
                    .await;

                for e in &mut u.expiries {
                    // Divergence events: occasionally shift a whole expiry smile.
                    // Magnitudes tuned so demo edges look like real vol
                    // dislocations (0.5–1.8 vp), not fantasy: wide drifts
                    // compound through multi-hop cycles into absurd boxes.
                    if rng.gen::<f64>() < dt_s / 240.0 {
                        let mag = 0.5 + 1.3 * rng.gen::<f64>();
                        e.drift_vp = if rng.gen::<f64>() < 0.5 { -mag } else { mag };
                        e.drift_tau_ms = rng.gen_range(150_000.0..300_000.0);
                    }
                    e.drift_vp *= (-dt_s * 1000.0 / e.drift_tau_ms).exp();

                    let t = ((e.expiry_ms - now).max(1) as f64 / YEAR_MS).max(1e-4);
                    let mut ticks: Vec<InstrTick> = Vec::with_capacity(64);
                    for inst in &mut e.insts {
                        // Publish most instruments every tick — a 30 % lottery
                        // starves multi-leg routes of quotes inside their
                        // fire window and turns every cycle into an unwind.
                        if rng.gen::<f64>() > 0.92 {
                            continue;
                        }
                        inst.noise_vp += -inst.noise_vp * (dt_s * 1000.0 / inst.hl_ms)
                            + inst.vov_vp * dt_s.sqrt() * normal(&mut rng);
                        if rng.gen::<f64>() < 0.05 {
                            inst.size_bid = 5.0 + rng.gen::<f64>().powi(2) * 250.0;
                            inst.size_ask = 5.0 + rng.gen::<f64>().powi(2) * 250.0;
                        }
                        // Skew: put-tilted smile.
                        let skew = -5.0 * inst.m * if inst.key.right == RIGHT_PUT { 1.3 } else { 1.0 }
                            + 8.0 * inst.m * inst.m;
                        let iv = e.base_vp + e.venue_off_vp + e.drift_vp + skew + inst.noise_vp;
                        if !(10.0..=300.0).contains(&iv) {
                            continue;
                        }
                        let hs = inst.hs_vp;
                        // Vol→price conversion so the engine's own solver round-trips.
                        let px = |vol: f64| {
                            bs_price(
                                spot,
                                inst.key.strike(),
                                t,
                                vol / 100.0,
                                if inst.key.right == RIGHT_CALL {
                                    ivarb_quant::Right::Call
                                } else {
                                    ivarb_quant::Right::Put
                                },
                            )
                            .unwrap_or(0.0)
                        };
                        ticks.push(InstrTick {
                            key: inst.key,
                            iv_mark_vp: Some(iv),
                            iv_bid_vp: Some((iv - hs).max(1.0)),
                            iv_ask_vp: Some(iv + hs),
                            px_mark_unit: Some(px(iv)),
                            px_bid_unit: Some(px((iv - hs).max(1.0))),
                            px_ask_unit: Some(px(iv + hs)),
                            size_bid: Some(inst.size_bid),
                            size_ask: Some(inst.size_ask),
                            spot: Some(spot),
                            contract_size: Some(1.0),
                            venue_ts_ms: Some(now - rand_range(400) as i64),
                        });
                    }
                    if !ticks.is_empty() {
                        let _ = ctx
                            .tx
                            .send(VenueMsg::Ticks {
                                venue: venue_id.clone(),
                                ticks,
                            })
                            .await;
                    }
                }
            }
        }
    }
}

fn date_from_ms(ms: i64) -> Option<u32> {
    use chrono::{Datelike, TimeZone};
    let dt = chrono::Utc.timestamp_millis_opt(ms).single()?;
    let d = dt.date_naive();
    Some(d.year() as u32 * 10_000 + d.month() * 100 + d.day())
}

/// Spawn the full sim venue set (used for Auto fallback and Always mode).
pub async fn spawn_sims(ctx: &VenueCtx, ids: &[&str]) {
    for id in ids {
        let base = ctx
            .cfg
            .venues
            .iter()
            .find(|v| v.id == *id)
            .cloned()
            .unwrap_or_else(|| ctx.spec.clone());
        let mut spec = base;
        spec.id = format!("sim:{id}");
        let sim_ctx = VenueCtx {
            spec,
            ..ctx.clone()
        };
        let venue = SimVenue::new(id.to_string());
        tokio::spawn(super::supervise(Box::new(venue), sim_ctx));
    }
}
