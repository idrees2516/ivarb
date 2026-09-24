//! Genetic-algorithm strategy optimizer.
//!
//! The engine records every opportunity lifecycle into a **tape** (episodes of
//! spread samples). A dedicated OS thread continuously replays that tape
//! against a population of strategy **genomes** — vectors of bounded real
//! parameters covering entry gates, exit behavior, patience, hold windows,
//! Kelly fraction, fee buffers, theta accounting, staleness limits and a
//! smooth time-of-day gate shape — and evolves them with tournament
//! selection, blend crossover, adaptive Gaussian mutation and elitism.
//!
//! Fitness is *risk-adjusted replay P&L*: Σ captured vp per trade (after
//! fee buffers, theta burn over the realized holding time and single-leg
//! risk) minus a drawdown penalty and a churn penalty. When a genome beats
//! the hand-tuned default by a margin, it can be **applied** to the live
//! strategy (engine gates/patience/hold, executor Kelly sizing, multi-hop
//! fee buffer, hold-lab EV threshold) — everything stays observable and
//! revertible from the Evolution Lab UI.
//!
//! The tape keeps the most recent ~6 h of episodes; a generation replays the
//! newest 8 000 episodes × 48 genomes in tens of milliseconds on one core,
//! so evolution keeps pace with the market without touching the hot path.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender};
use ivarb_protocol::{GaCommand, GaGene, GaStatus};

use crate::timing::ThetaBucket;

// ---------------------------------------------------------------------------
// Genome definition
// ---------------------------------------------------------------------------

pub const N_GENES: usize = 12;

pub const G_ENTRY_GATE_MULT: usize = 0;
pub const G_EXIT_MULT: usize = 1;
pub const G_PATIENCE_MULT: usize = 2;
pub const G_HOLD_MULT: usize = 3;
pub const G_KELLY_FRAC: usize = 4;
pub const G_FEE_BUFFER: usize = 5;
pub const G_THETA_NET: usize = 6;
pub const G_STALE_CAP_MS: usize = 7;
pub const G_TOD_AMP: usize = 8;
pub const G_TOD_PEAK_H: usize = 9;
pub const G_HOLD_EV_GATE_USD: usize = 10;
pub const G_MH_FEE_BUFFER: usize = 11;

/// Gene metadata: (key, label, lo, hi, default).
pub const GENE_SPEC: [(&str, &str, f64, f64, f64); N_GENES] = [
    ("entry_gate_mult", "Entry-gate multiplier", 0.6, 2.5, 1.0),
    ("exit_mult", "Exit fraction of entry edge", 0.2, 1.0, 0.5),
    ("patience_mult", "Patience-window multiplier", 0.3, 3.0, 1.0),
    ("hold_mult", "Max-hold multiplier", 0.3, 3.0, 1.0),
    ("kelly_frac", "Kelly sizing fraction", 0.05, 0.6, 0.25),
    ("fee_buffer", "Fee buffer ×", 1.0, 1.6, 1.0),
    ("theta_net", "Netted-theta factor", 0.05, 0.5, 0.15),
    ("stale_cap_ms", "Snipe staleness cap (ms)", 500.0, 20_000.0, 3_000.0),
    ("tod_amp", "Time-of-day gate amplitude", -0.4, 0.4, 0.0),
    ("tod_peak_h", "Time-of-day peak hour (UTC)", 0.0, 24.0, 14.0),
    ("hold_ev_gate", "Hold-lab EV gate ($/contract)", 0.0, 30.0, 5.0),
    ("mh_fee_buffer", "Multi-hop fee buffer ×", 0.8, 1.5, 1.0),
];

pub type Genome = [f64; N_GENES];

pub fn default_genome() -> Genome {
    let mut g = [0.0; N_GENES];
    for (i, (_, _, _, _, d)) in GENE_SPEC.iter().enumerate() {
        g[i] = *d;
    }
    g
}

fn clamp_gene(i: usize, v: f64) -> f64 {
    let (_, _, lo, hi, _) = GENE_SPEC[i];
    v.clamp(lo, hi)
}

pub fn genome_genes(g: &Genome) -> Vec<GaGene> {
    GENE_SPEC
        .iter()
        .enumerate()
        .map(|(i, (key, label, lo, hi, d))| GaGene {
            key: key.to_string(),
            label: label.to_string(),
            value: g[i],
            lo: *lo,
            hi: *hi,
            default: *d,
        })
        .collect()
}

/// Time-of-day gate multiplier from the diurnal genes: a raised-cosine bump
/// peaking (gate ×1+amp) 12 h away from `tod_peak_h` and lowest at the peak
/// itself — amp > 0 encodes "trade more in the peak-liquidity hour, demand
/// extra edge in the thin hours".
pub fn tod_gate_mult(g: &Genome, hour_utc: f64) -> f64 {
    let amp = g[G_TOD_AMP];
    let peak = g[G_TOD_PEAK_H];
    let d = (hour_utc - peak) / 24.0;
    let cos = (2.0 * std::f64::consts::PI * d).cos();
    1.0 + amp * (1.0 - cos) / 2.0
}

/// Effective entry gate (vp) under a genome.
pub fn effective_gate(g: &Genome, base_gate_vp: f64, dte: f64, hour_utc: f64) -> f64 {
    let bucket = ThetaBucket::from_dte(dte);
    base_gate_vp * bucket.gate_mult() * g[G_ENTRY_GATE_MULT] * tod_gate_mult(g, hour_utc)
}

// ---------------------------------------------------------------------------
// Tape
// ---------------------------------------------------------------------------

/// One recorded spread sample of an opportunity episode.
#[derive(Debug, Clone, Copy)]
pub struct TapeSample {
    pub t_ms: i64,
    /// Net vp after entry fees.
    pub net_vp: f64,
    /// Round-trip fee vp at record time.
    pub fee_vp: f64,
    /// Theta burn, vp/day.
    pub theta_vp_day: f64,
    pub dte: f64,
    pub exec_mode: bool,
    pub quote_age_ms: f64,
    /// Base gate at record time (config min_net_vp).
    pub gate_base: f64,
}

/// One episode: the lifecycle of one opportunity id.
#[derive(Debug, Clone)]
pub struct Episode {
    pub id: String,
    pub samples: VecDeque<TapeSample>,
}

/// Events the engine pushes into the GA tape.
#[derive(Debug, Clone)]
pub enum TapeEvent {
    Sample { id: String, s: TapeSample },
    End { id: String },
}

const MAX_EPISODES: usize = 16_000;
const MAX_SAMPLES_PER_EP: usize = 900;
const REPLAY_EPISODES: usize = 8_000;

// ---------------------------------------------------------------------------
// Replay fitness model
// ---------------------------------------------------------------------------

struct ReplayResult {
    fitness: f64,
    pnl_vp: f64,
    max_dd_vp: f64,
    trades: u32,
}

fn replay(eps: &[Episode], g: &Genome) -> ReplayResult {
    let mut pnl = 0.0;
    let mut peak = 0.0;
    let mut max_dd = 0.0;
    let mut trades = 0u32;
    let fee_buf = g[G_FEE_BUFFER];
    let theta_net = g[G_THETA_NET];

    for ep in eps {
        let mut entry: Option<TapeSample> = None;
        let mut patience_ms = 0.0;
        for s in &ep.samples {
            let hour = ((s.t_ms / 3_600_000).rem_euclid(24)) as f64;
            if let Some(en) = entry {
                // Exit condition: spread fell to exit fraction of entry edge.
                let exit_level = en.net_vp * g[G_EXIT_MULT];
                let held_ms = (s.t_ms - en.t_ms) as f64;
                if s.net_vp <= exit_level || held_ms >= patience_ms || s.quote_age_ms > g[G_STALE_CAP_MS] * 10.0 {
                    let captured = (en.net_vp - s.net_vp) - s.fee_vp * fee_buf;
                    let held_days = held_ms / 86_400_000.0;
                    let theta_burn = en.theta_vp_day * held_days * theta_net;
                    let p_fill = if en.quote_age_ms < g[G_STALE_CAP_MS] { 0.78 } else { 0.5 };
                    let single_leg = (1.0 - p_fill) * (s.fee_vp + 0.4);
                    pnl += captured - theta_burn - single_leg;
                    trades += 1;
                    entry = None;
                    continue;
                }
            } else {
                // Entry condition.
                let gate = effective_gate(g, s.gate_base, s.dte, hour);
                let fresh_enough = s.exec_mode && s.quote_age_ms <= g[G_STALE_CAP_MS];
                if s.net_vp >= gate && fresh_enough {
                    entry = Some(*s);
                    let bucket = ThetaBucket::from_dte(s.dte);
                    patience_ms = bucket.patience_s() * 1000.0 * g[G_PATIENCE_MULT];
                }
            }
        }
        // Episode ended with an open trade → forced exit at the last sample.
        if let Some(en) = entry {
            if let Some(last) = ep.samples.back() {
                let captured = (en.net_vp - last.net_vp) - last.fee_vp * fee_buf - 0.15;
                pnl += captured;
                trades += 1;
            }
        }
        // Drawdown tracking on the cumulative curve.
        if pnl > peak {
            peak = pnl;
        }
        let dd = peak - pnl;
        if dd > max_dd {
            max_dd = dd;
        }
    }

    // Risk adjustment: drawdown penalty + churn penalty, normalized per
    // 100 episodes so fitness is comparable across tape sizes.
    let n = eps.len().max(1) as f64;
    let churn = trades as f64 * 0.01;
    let fitness = (pnl - 0.5 * max_dd - churn) / n * 100.0;
    ReplayResult {
        fitness,
        pnl_vp: pnl,
        max_dd_vp: max_dd,
        trades,
    }
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Everything the rest of the system reads from the GA (wait-free).
pub struct GaShared {
    pub status: GaStatus,
    pub genome: Genome,
}

#[derive(Clone)]
pub struct GaHandle {
    pub shared: Arc<ArcSwap<GaShared>>,
    pub cmd_tx: Sender<GaCommand>,
    pub tape_tx: Sender<TapeEvent>,
    pub applied: Arc<AtomicBool>,
}

// ---------------------------------------------------------------------------
// GA thread
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Individual {
    genome: Genome,
    fitness: f64,
}

pub fn spawn_ga(gen_ms: u64, applied_default: bool) -> GaHandle {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<GaCommand>();
    let (tape_tx, tape_rx) = crossbeam_channel::unbounded::<TapeEvent>();
    let shared = Arc::new(ArcSwap::from_pointee(GaShared {
        status: GaStatus {
            t_ms: 0,
            generation: 0,
            running: true,
            applied: applied_default,
            best_fitness: 0.0,
            default_fitness: 0.0,
            improvement_pct: 0.0,
            genes: genome_genes(&default_genome()),
            history: Vec::new(),
            tape_episodes: 0,
            replay_trades: 0,
            replay_pnl_vp: 0.0,
            replay_max_dd_vp: 0.0,
            gen_ms,
        },
        genome: default_genome(),
    }));
    let applied = Arc::new(AtomicBool::new(applied_default));

    {
        let shared = shared.clone();
        let applied = applied.clone();
        std::thread::Builder::new()
            .name("ivarb-ga".into())
            .spawn(move || run(gen_ms, shared, applied, cmd_rx, tape_rx))
            .expect("spawn ga thread");
    }

    GaHandle {
        shared,
        cmd_tx,
        tape_tx,
        applied,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn run(
    mut gen_ms: u64,
    shared: Arc<ArcSwap<GaShared>>,
    applied: Arc<AtomicBool>,
    cmd_rx: Receiver<GaCommand>,
    tape_rx: Receiver<TapeEvent>,
) {
    const POP: usize = 48;
    const ELITE: usize = 3;
    let mut rng = ivarb_quant::Rng::from_entropy();
    let mut episodes: VecDeque<Episode> = VecDeque::with_capacity(MAX_EPISODES);
    let mut open: std::collections::HashMap<String, Episode> = std::collections::HashMap::new();

    // Seed the population: 1 default + perturbations.
    let mut pop: Vec<Individual> = Vec::with_capacity(POP);
    let def = default_genome();
    pop.push(Individual {
        genome: def,
        fitness: f64::NEG_INFINITY,
    });
    for _ in 1..POP {
        let mut g = def;
        for i in 0..N_GENES {
            let (_, _, lo, hi, _) = GENE_SPEC[i];
            let range = hi - lo;
            g[i] = clamp_gene(i, g[i] + rng.next_normal_clipped(range * 0.15));
        }
        pop.push(Individual {
            genome: g,
            fitness: f64::NEG_INFINITY,
        });
    }

    let mut generation: u32 = 0;
    let mut mutation_scale: f64 = 0.15;
    let mut history: Vec<f64> = Vec::new();
    let mut running = true;

    let mut last_gen = Instant::now();
    loop {
        // ---- drain commands ----
        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                GaCommand::Pause => running = false,
                GaCommand::Resume => running = true,
                GaCommand::SetApplied { applied: a } => applied.store(a, Ordering::Relaxed),
                GaCommand::Reset => {
                    for (idx, ind) in pop.iter_mut().enumerate() {
                        if idx == 0 {
                            ind.genome = default_genome();
                        } else {
                            let mut g = default_genome();
                            for i in 0..N_GENES {
                                let (_, _, lo, hi, _) = GENE_SPEC[i];
                                g[i] = clamp_gene(i, g[i] + rng.next_normal_clipped((hi - lo) * 0.15));
                            }
                            ind.genome = g;
                        }
                        ind.fitness = f64::NEG_INFINITY;
                    }
                    generation = 0;
                    history.clear();
                    mutation_scale = 0.15;
                }
                GaCommand::SetGenMs { ms } => {
                    if ms >= 1000 {
                        gen_ms = ms;
                    }
                }
            }
        }

        // ---- drain tape events ----
        let mut drained = 0;
        while drained < 20_000 {
            match tape_rx.try_recv() {
                Ok(TapeEvent::Sample { id, s }) => {
                    let ep = open.entry(id).or_insert_with(|| Episode {
                        id: String::new(),
                        samples: VecDeque::new(),
                    });
                    if ep.samples.len() < MAX_SAMPLES_PER_EP {
                        ep.samples.push_back(s);
                    }
                    drained += 1;
                }
                Ok(TapeEvent::End { id }) => {
                    if let Some(mut ep) = open.remove(&id) {
                        if ep.samples.len() >= 4 {
                            ep.id = id;
                            episodes.push_back(ep);
                            while episodes.len() > MAX_EPISODES {
                                episodes.pop_front();
                            }
                        }
                    }
                    drained += 1;
                }
                Err(_) => break,
            }
        }

        // ---- evolve ----
        if running && last_gen.elapsed() >= Duration::from_millis(gen_ms) && episodes.len() >= 24 {
            last_gen = Instant::now();
            generation += 1;
            mutation_scale = (mutation_scale * 0.985).max(0.03);

            let tape: Vec<Episode> = episodes
                .iter()
                .rev()
                .take(REPLAY_EPISODES)
                .rev()
                .cloned()
                .collect();

            for ind in pop.iter_mut() {
                ind.fitness = replay(&tape, &ind.genome).fitness;
            }
            pop.sort_by(|a, b| b.fitness.partial_cmp(&a.fitness).unwrap_or(std::cmp::Ordering::Equal));

            // Produce the next generation.
            let mut next: Vec<Individual> = pop[..ELITE.min(pop.len())].to_vec();
            while next.len() < POP {
                // Tournament selection ×2.
                let pick = |rng: &mut ivarb_quant::Rng| -> Genome {
                    let mut best_i = rng.gen_range_usize(pop.len());
                    for _ in 0..2 {
                        let c = rng.gen_range_usize(pop.len());
                        if pop[c].fitness > pop[best_i].fitness {
                            best_i = c;
                        }
                    }
                    pop[best_i].genome
                };
                let pa = pick(&mut rng);
                let pb = pick(&mut rng);
                // Blend crossover.
                let mut child = pa;
                if rng.bernoulli(0.75) {
                    for i in 0..N_GENES {
                        let alpha = rng.next_range(0.35, 0.65);
                        child[i] = alpha.mul_add(pa[i], (1.0 - alpha) * pb[i]);
                    }
                }
                // Gaussian mutation.
                for i in 0..N_GENES {
                    if rng.bernoulli(0.10) {
                        let (_, _, lo, hi, _) = GENE_SPEC[i];
                        child[i] = clamp_gene(i, child[i] + rng.next_normal_clipped((hi - lo) * mutation_scale));
                    }
                }
                next.push(Individual {
                    genome: child,
                    fitness: f64::NEG_INFINITY,
                });
            }
            pop = next;

            // Publish status.
            let best = &pop[0];
            let default_fit = replay(&tape, &def).fitness;
            let best_res = replay(&tape, &best.genome);
            history.push(best.fitness);
            if history.len() > 240 {
                history.remove(0);
            }
            let improvement = if default_fit.abs() > 1e-9 {
                (best.fitness - default_fit) / default_fit.abs() * 100.0
            } else {
                0.0
            };
            let applied_now = applied.load(Ordering::Relaxed);
            shared.store(Arc::new(GaShared {
                status: GaStatus {
                    t_ms: now_ms(),
                    generation,
                    running,
                    applied: applied_now,
                    best_fitness: best.fitness,
                    default_fitness: default_fit,
                    improvement_pct: improvement,
                    genes: genome_genes(&best.genome),
                    history: history.clone(),
                    tape_episodes: episodes.len() as u32,
                    replay_trades: best_res.trades,
                    replay_pnl_vp: best_res.pnl_vp,
                    replay_max_dd_vp: best_res.max_dd_vp,
                    gen_ms,
                },
                genome: best.genome,
            }));
            // The applied flag gates live use; when on, the published genome
            // is the best of the latest generation.
        } else if last_gen.elapsed() >= Duration::from_millis(gen_ms.max(500)) {
            last_gen = Instant::now();
            // Not enough tape yet — still refresh timestamps.
            let cur = shared.load();
            let mut st = cur.status.clone();
            st.t_ms = now_ms();
            st.running = running;
            st.tape_episodes = episodes.len() as u32;
            st.gen_ms = gen_ms;
            let genome = cur.genome;
            drop(cur);
            shared.store(Arc::new(GaShared { status: st, genome }));
        }

        std::thread::sleep(Duration::from_millis(250));
    }
}

// ---------------------------------------------------------------------------
// Genome view used by the engine hot path
// ---------------------------------------------------------------------------

/// Snapshot of the currently applied genome (or defaults when not applied).
#[derive(Debug, Clone, Copy)]
pub struct GenomeView {
    pub genome: Genome,
    pub applied: bool,
    pub generation: u32,
}

impl Default for GenomeView {
    fn default() -> Self {
        Self {
            genome: default_genome(),
            applied: false,
            generation: 0,
        }
    }
}

impl GenomeView {
    pub fn from_shared(shared: &ArcSwap<GaShared>, applied: &AtomicBool) -> Self {
        let sh = shared.load();
        let on = applied.load(Ordering::Relaxed);
        Self {
            genome: sh.genome,
            applied: on,
            generation: sh.status.generation,
        }
    }

    /// Entry-gate multiplier to apply (1.0 when not applied).
    #[inline]
    pub fn gate_mult(&self, hour_utc: f64) -> f64 {
        if !self.applied {
            return 1.0;
        }
        tod_gate_mult(&self.genome, hour_utc) * self.genome[G_ENTRY_GATE_MULT]
    }

    #[inline]
    pub fn patience_mult(&self) -> f64 {
        if self.applied {
            self.genome[G_PATIENCE_MULT]
        } else {
            1.0
        }
    }

    #[inline]
    pub fn hold_mult(&self) -> f64 {
        if self.applied {
            self.genome[G_HOLD_MULT]
        } else {
            1.0
        }
    }

    #[inline]
    pub fn kelly_frac(&self) -> f64 {
        if self.applied {
            self.genome[G_KELLY_FRAC]
        } else {
            0.25
        }
    }

    #[inline]
    pub fn mh_fee_buffer(&self) -> f64 {
        if self.applied {
            self.genome[G_MH_FEE_BUFFER]
        } else {
            1.0
        }
    }

    #[inline]
    pub fn hold_ev_gate_usd(&self) -> f64 {
        if self.applied {
            self.genome[G_HOLD_EV_GATE_USD]
        } else {
            5.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(t_ms: i64, net: f64) -> TapeSample {
        TapeSample {
            t_ms,
            net_vp: net,
            fee_vp: 0.10,
            theta_vp_day: 2.0,
            dte: 20.0,
            exec_mode: true,
            quote_age_ms: 500.0,
            gate_base: 0.5,
        }
    }

    fn episode(nets: &[f64]) -> Episode {
        let mut samples = VecDeque::new();
        for (i, n) in nets.iter().enumerate() {
            samples.push_back(sample(i as i64 * 5_000, *n));
        }
        Episode {
            id: "test".into(),
            samples,
        }
    }

    #[test]
    fn high_gate_avoids_junk_episodes() {
        // Episodes alternate: junk (peaks 0.55 then collapses to 0) and gold
        // (peaks 2.0, converges to 0.2). A gate of 0.5 takes both; the junk
        // one loses money. A gate of 1.5 takes only gold.
        let mut eps = Vec::new();
        for k in 0..40 {
            if k % 2 == 0 {
                eps.push(episode(&[0.55, 0.4, 0.2, 0.0]));
            } else {
                eps.push(episode(&[1.2, 2.0, 1.2, 0.2]));
            }
        }
        let mut permissive = default_genome();
        permissive[G_ENTRY_GATE_MULT] = 0.6; // gate ≈ 0.3 → takes junk early
        let mut strict = default_genome();
        strict[G_ENTRY_GATE_MULT] = 2.2; // gate ≈ 1.1 → waits for the 2.0 peak

        let r_loose = replay(&eps, &permissive);
        let r_strict = replay(&eps, &strict);
        assert!(
            r_strict.fitness > r_loose.fitness,
            "strict {} vs loose {}",
            r_strict.fitness,
            r_loose.fitness
        );
    }

    #[test]
    fn genome_bounds_respected() {
        let mut rng = ivarb_quant::Rng::new(3);
        for _ in 0..1_000 {
            let mut g = default_genome();
            for i in 0..N_GENES {
                let (_, _, lo, hi, _) = GENE_SPEC[i];
                g[i] = clamp_gene(i, rng.next_range(-10.0, 10.0));
                assert!(g[i] >= lo && g[i] <= hi);
            }
        }
    }

    #[test]
    fn tod_shape_is_bounded() {
        let mut g = default_genome();
        g[G_TOD_AMP] = 0.4;
        g[G_TOD_PEAK_H] = 14.0;
        for h in 0..24 {
            let m = tod_gate_mult(&g, h as f64);
            assert!((0.6..=1.4).contains(&m), "hour {h} mult {m}");
        }
    }
}
