//! recv_stats : récepteur OVP minimal, pour mesurer le protocole sans le player.
//!
//! Écoute les paquets "OV" sur UDP 5000, réassemble les images, envoie HELLO / STATS /
//! KEYFRAME_REQUEST à l'émetteur (UDP 5003) et écrit un résumé par seconde. Il ne décode
//! rien : avec `--dump fichier.h264`, les images livrées sont écrites telles quelles
//! (Annex B), lisibles par `ffplay fichier.h264`.
//!
//! ```text
//! cargo run -p orbit-protocol --example recv_stats --release -- --host 192.168.1.107
//!     [--port 5000] [--feedback-port 5003] [--on-loss freeze|show]
//!     [--frame-lost-timeout-ms 20] [--simulate-loss <pct>] [--seed 1] [--dump <fichier>]
//!     [--duration-s <s>]
//! ```
//!
//! Mesure clé pour le pacing : l'**étalement à l'arrivée** (premier fragment → image complète).
//! Sans pacing (rafale), il tombe à quelques centaines de µs ; avec, il approche la cible
//! affichée par orbit-sender.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use orbit_protocol::feedback::{self, Feedback, StatsReport};
use orbit_protocol::packet::{RejectReason, VideoHeader};
use orbit_protocol::reassembly::{
    Event, LossPolicy, Reassembler, ReassemblerConfig, ReassemblyStats,
};
use orbit_protocol::rng::SplitMix64;

struct Args {
    host: IpAddr,
    port: u16,
    feedback_port: u16,
    policy: LossPolicy,
    frame_lost_timeout_ms: u64,
    simulate_loss: f64,
    seed: u64,
    dump: Option<String>,
    trace_losses: bool,
    duration_s: Option<f64>,
}

fn parse_args() -> Result<Args, String> {
    let mut host = None;
    let mut a = Args {
        host: IpAddr::from([0, 0, 0, 0]),
        port: 5000,
        feedback_port: 5003,
        policy: LossPolicy::Freeze,
        frame_lost_timeout_ms: 20,
        simulate_loss: 0.0,
        seed: 1,
        dump: None,
        trace_losses: false,
        duration_s: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(opt) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("valeur manquante après {opt}"));
        let err = |e: &dyn std::fmt::Display| format!("{opt} : {e}");
        match opt.as_str() {
            "--host" => host = Some(val()?.parse::<IpAddr>().map_err(|e| err(&e))?),
            "--port" => a.port = val()?.parse().map_err(|e| err(&e))?,
            "--feedback-port" => a.feedback_port = val()?.parse().map_err(|e| err(&e))?,
            "--on-loss" => {
                a.policy = match val()?.as_str() {
                    "freeze" => LossPolicy::Freeze,
                    "show" => LossPolicy::Show,
                    v => return Err(format!("--on-loss : freeze ou show, pas « {v} »")),
                }
            }
            "--frame-lost-timeout-ms" => {
                a.frame_lost_timeout_ms = val()?.parse().map_err(|e| err(&e))?
            }
            "--simulate-loss" => a.simulate_loss = val()?.parse().map_err(|e| err(&e))?,
            "--seed" => a.seed = val()?.parse().map_err(|e| err(&e))?,
            "--dump" => a.dump = Some(val()?),
            "--trace-losses" => a.trace_losses = true,
            "--duration-s" => a.duration_s = Some(val()?.parse().map_err(|e| err(&e))?),
            "-h" | "--help" => {
                return Err("usage : recv_stats --host <IP du PC fixe> [--port 5000] \
                    [--feedback-port 5003] [--on-loss freeze|show] [--frame-lost-timeout-ms 20] \
                    [--simulate-loss <pct>] [--seed 1] [--dump <fichier>] [--duration-s <s>]"
                    .into());
            }
            autre => return Err(format!("option inconnue : {autre}")),
        }
    }
    a.host = host.ok_or("--host est obligatoire (IP du PC fixe)")?;
    Ok(a)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = run(args) {
        eprintln!("erreur : {e}");
        std::process::exit(1);
    }
}

/// Compteurs d'arrivée propres à recv_stats (étalement, débit).
#[derive(Default)]
struct Window {
    bytes: u64,
    spread_sum_us: u64,
    spread_max_us: u64,
    frames: u64,
    ignored_source: u64,
    simulated_drops: u64,
}

fn run(args: Args) -> std::io::Result<()> {
    let socket = bind_large_buffer(args.port)?;
    // Réception dans un thread à part, en lecture bloquante SANS délai : sous Windows, un
    // recv_from qui expire (SO_RCVTIMEO) peut perdre le datagramme qui arrive au même instant
    // (« socket in an indeterminate state », doc Winsock). Mesuré : ~1 fragment perdu par
    // seconde, souvent le premier d'une image, rien qu'en boucle locale.
    let (tx_paquets, rx_paquets) = std::sync::mpsc::sync_channel::<(Vec<u8>, SocketAddr)>(65_536);
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        loop {
            match socket.recv_from(&mut buf) {
                Ok((n, source)) => {
                    if tx_paquets.send((buf[..n].to_vec(), source)).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
                Err(e) => {
                    println!("réception : {e}");
                    return;
                }
            }
        }
    });
    let retour = UdpSocket::bind(("0.0.0.0", 0))?;
    let dest_retour = SocketAddr::new(args.host, args.feedback_port);
    let mut dump = match &args.dump {
        Some(p) => Some(BufWriter::new(File::create(p)?)),
        None => None,
    };

    let mut re = Reassembler::new(ReassemblerConfig {
        policy: args.policy,
        frame_lost_timeout_us: args.frame_lost_timeout_ms * 1000,
        ..ReassemblerConfig::default()
    });
    let mut rng = SplitMix64::new(args.seed);
    let debut = Instant::now();
    let now_us = || debut.elapsed().as_micros() as u64;

    println!(
        "recv_stats : écoute UDP {} (source acceptée : {}), retour vers {dest_retour}, \
         politique {:?}, délai de perte {} ms",
        args.port, args.host, args.policy, args.frame_lost_timeout_ms
    );

    let mut seq: u32 = 0;
    let mut envoyer = |m: &Feedback| {
        let d = feedback::encode(seq, m);
        seq = seq.wrapping_add(1);
        if let Err(e) = retour.send_to(&d, dest_retour) {
            println!("envoi du retour : {e}");
        }
    };

    let mut trace = LossTrace { enabled: args.trace_losses, frames: Default::default() };
    let mut fen = Window::default();
    let mut prochain_hello = Instant::now();
    let mut prochain_stats = Instant::now() + Duration::from_millis(250);
    let mut prochain_resume = Instant::now() + Duration::from_secs(1);
    let mut stats_envoyees = ReassemblyStats::default();
    let mut stats_resume = ReassemblyStats::default();

    let fin = args.duration_s.map(|d| Instant::now() + Duration::from_secs_f64(d.max(0.0)));
    loop {
        if fin.is_some_and(|f| Instant::now() >= f) {
            if let Some(w) = dump.as_mut() {
                w.flush()?;
            }
            return Ok(());
        }
        match rx_paquets.recv_timeout(Duration::from_millis(2)) {
            Ok((buf, source)) => {
                let n = buf.len();
                if source.ip() != args.host {
                    fen.ignored_source += 1;
                } else if args.simulate_loss > 0.0 && rng.chance_percent(args.simulate_loss) {
                    fen.simulated_drops += 1;
                } else {
                    // Un datagramme trop long est rejeté et compté par le réassembleur.
                    if args.trace_losses
                        && let Ok((h, _)) = VideoHeader::parse(&buf[..n])
                    {
                        trace.note(&h, now_us());
                    }
                    re.push(&buf[..n], now_us());
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(std::io::Error::other("thread de réception arrêté"));
            }
        }

        let t = now_us();
        while let Some(ev) = re.poll(t) {
            match ev {
                Event::Frame(f) => {
                    if f.fec_recovered {
                        trace.report(f.frame_id, "récupérée par FEC");
                    }
                    let etalement = f.completed_us - f.first_arrival_us;
                    fen.frames += 1;
                    fen.bytes += f.data.len() as u64;
                    fen.spread_sum_us += etalement;
                    fen.spread_max_us = fen.spread_max_us.max(etalement);
                    if let Some(w) = dump.as_mut() {
                        w.write_all(&f.data)?;
                    }
                }
                Event::Lost { frame_id } => {
                    println!("image {frame_id} perdue");
                    trace.report(frame_id, "perdue");
                }
            }
        }
        if let Some(id) = re.take_keyframe_request(t) {
            envoyer(&Feedback::KeyframeRequest { lost_frame_id: id });
        }

        let maintenant = Instant::now();
        if maintenant >= prochain_hello {
            envoyer(&Feedback::Hello);
            prochain_hello = maintenant + Duration::from_millis(500);
        }
        if maintenant >= prochain_stats {
            let s = re.stats().clone();
            envoyer(&Feedback::Stats(delta_report(&stats_envoyees, &s)));
            stats_envoyees = s;
            prochain_stats = maintenant + Duration::from_millis(250);
        }
        if maintenant >= prochain_resume {
            let s = re.stats().clone();
            println!("{}", resume(&stats_resume, &s, &fen));
            if let Some(w) = dump.as_mut() {
                w.flush()?;
            }
            stats_resume = s;
            fen = Window::default();
            prochain_resume = maintenant + Duration::from_secs(1);
        }
    }
}

/// Socket d'écoute avec un tampon de réception de 8 Mo : avec le tampon par défaut de
/// Windows (64 Ko), une image clé en rafale déborde et l'on mesurerait des pertes locales.
fn bind_large_buffer(port: u16) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    if let Err(e) = s.set_recv_buffer_size(8 * 1024 * 1024) {
        println!("tampon de réception de 8 Mo refusé : {e}");
    }
    s.bind(&SocketAddr::from(([0, 0, 0, 0], port)).into())?;
    Ok(s.into())
}

/// `--trace-losses` : pour chaque image récupérée par FEC ou perdue, quels fragments
/// manquaient. Des pertes groupées (indices consécutifs) trahissent des rafales du Wi-Fi.
struct LossTrace {
    enabled: bool,
    /// frame_id → (clé, fragments attendus par bloc, fragments reçus, heure du 1er fragment).
    frames: std::collections::BTreeMap<u32, (bool, Vec<Vec<bool>>, u64)>,
}

impl LossTrace {
    fn note(&mut self, h: &VideoHeader, now_us: u64) {
        if !self.enabled {
            return;
        }
        let e = self
            .frames
            .entry(h.frame_id)
            .or_insert_with(|| (h.keyframe, vec![Vec::new(); usize::from(h.block_count)], now_us));
        if let Some(bloc) = e.1.get_mut(usize::from(h.block_index)) {
            let total = usize::from(h.data_shards) + usize::from(h.parity_shards);
            if bloc.len() != total {
                *bloc = vec![false; total];
            }
            if let Some(v) = bloc.get_mut(usize::from(h.shard_index)) {
                *v = true;
            }
        }
        // Mémoire bornée : on oublie les images de plus de 2 s.
        self.frames.retain(|_, v| now_us.saturating_sub(v.2) < 2_000_000);
    }

    fn report(&mut self, frame_id: u32, quoi: &str) {
        if !self.enabled {
            return;
        }
        let Some((cle, blocs, _)) = self.frames.get(&frame_id) else {
            println!("  image {frame_id} {quoi} : aucun fragment reçu");
            return;
        };
        for (b, frag) in blocs.iter().enumerate() {
            let manquants: Vec<usize> =
                frag.iter().enumerate().filter(|(_, r)| !**r).map(|(i, _)| i).collect();
            if !manquants.is_empty() || frag.is_empty() {
                println!(
                    "  image {frame_id}{} {quoi} : bloc {b}, {} fragments, manquants {:?}",
                    if *cle { " (clé)" } else { "" },
                    frag.len(),
                    manquants
                );
            }
        }
    }
}

fn d32(a: u64, b: u64) -> u32 {
    b.saturating_sub(a).min(u64::from(u32::MAX)) as u32
}

/// Paquet STATS : compteurs depuis le dernier envoi.
fn delta_report(avant: &ReassemblyStats, apres: &ReassemblyStats) -> StatsReport {
    StatsReport {
        last_frame_id: apres.last_frame_id,
        frames_ok: d32(avant.frames_ok, apres.frames_ok),
        frames_fec_recovered: d32(avant.frames_fec_recovered, apres.frames_fec_recovered),
        frames_lost: d32(avant.frames_lost, apres.frames_lost),
        shards_received: d32(avant.shards_received, apres.shards_received),
        shards_lost: d32(avant.shards_lost, apres.shards_lost),
        jitter_us: apres.jitter_us,
    }
}

fn resume(avant: &ReassemblyStats, s: &ReassemblyStats, f: &Window) -> String {
    let d = |a: u64, b: u64| b.saturating_sub(a);
    let recus = d(avant.shards_received, s.shards_received);
    let perdus = d(avant.shards_lost, s.shards_lost);
    let taux =
        if recus + perdus > 0 { 100.0 * perdus as f64 / (recus + perdus) as f64 } else { 0.0 };
    let moy = |somme: u64| if f.frames > 0 { somme as f64 / f.frames as f64 / 1000.0 } else { 0.0 };
    let rejets: Vec<String> = RejectReason::ALL
        .iter()
        .filter_map(|r| {
            let n = d(avant.rejected[r.index()], s.rejected[r.index()]);
            (n > 0).then(|| format!("{}={n}", r.name()))
        })
        .collect();
    format!(
        "images ok {} fec {} perdues {} retard {} sautées {} | {:.1} Mbit/s utiles | fragments \
         reçus {recus} perdus {perdus} ({taux:.2} %) doublons {} | étalement moy {:.2} ms max {:.2} \
         | gigue {} µs | demandes clé {} | resync {} | rejets {} | autres sources {} | pertes \
         simulées {}{}",
        d(avant.frames_ok, s.frames_ok),
        d(avant.frames_fec_recovered, s.frames_fec_recovered),
        d(avant.frames_lost, s.frames_lost),
        d(avant.frames_late, s.frames_late),
        d(avant.frames_skipped, s.frames_skipped),
        f.bytes as f64 * 8.0 / 1e6,
        d(avant.shards_duplicate, s.shards_duplicate),
        moy(f.spread_sum_us),
        f.spread_max_us as f64 / 1000.0,
        s.jitter_us,
        d(avant.keyframe_requests, s.keyframe_requests),
        d(avant.resyncs, s.resyncs),
        if rejets.is_empty() { "0".to_string() } else { rejets.join(" ") },
        f.ignored_source,
        f.simulated_drops,
        if f.frames == 0 {
            " | (aucune image : émetteur arrêté, en pause, ou attente d'image clé)"
        } else {
            ""
        },
    )
}
