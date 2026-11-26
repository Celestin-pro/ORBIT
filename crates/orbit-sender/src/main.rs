//! orbit-sender : émetteur du protocole vidéo ORBIT (OVP v1), côté PC fixe.
//!
//! Lance ffmpeg (capture + encodage, H.264/HEVC Annex B sur stdout), découpe le flux en
//! images, ajoute la parité Reed-Solomon, étale l'envoi (pacing) vers le portable et écoute
//! son canal retour. Spécification : PROTOCOL.md.
//!
//! Threads : lecteur (stdout ffmpeg), découpeur, envoi, retour, stderr ffmpeg, et le thread
//! principal qui écrit un résumé par seconde et gère Ctrl+C.

// `unsafe` interdit partout sauf dans timer.rs (appels Win32 du pacing).
#![deny(unsafe_code)]

mod config;
mod feedback;
mod ffmpeg;
mod pacing;
mod pipeline;
mod stats;
#[allow(unsafe_code)]
mod timer;

use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use orbit_protocol::Codec;
use orbit_protocol::fragment::{FecParams, Fragmenter};

use crate::config::{CodecArg, Config, SplitMode};
use crate::pacing::PacingParams;
use crate::pipeline::{SenderSettings, Shared, SplitterSettings};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        // Sur stdout : `orbit-sender ... | Tee-Object x.log` fonctionne tel quel sous PowerShell.
        .target(env_logger::Target::Stdout)
        .init();
    match run(Config::parse()) {
        Ok(()) => {}
        Err(e) => {
            log::error!("{e:#}");
            std::process::exit(1);
        }
    }
}

fn run(cfg: Config) -> anyhow::Result<()> {
    let codec = match cfg.codec {
        CodecArg::H264 => Codec::H264,
        CodecArg::Hevc => Codec::Hevc,
    };
    anyhow::ensure!(cfg.fps > 0, "--fps doit être > 0");
    anyhow::ensure!(cfg.bitrate_mbps > 0.0, "--bitrate-mbps doit être > 0");
    let fragmenter =
        Fragmenter::new(FecParams { percent: cfg.fec_percent, min_parity: cfg.fec_min_parity })
            .context("réglages FEC")?;

    // Sockets d'abord : une erreur de port doit arrêter avant de lancer ffmpeg.
    let feedback_socket = UdpSocket::bind(("0.0.0.0", cfg.feedback_port))
        .with_context(|| format!("écoute du canal retour sur UDP {}", cfg.feedback_port))?;
    let video_socket = UdpSocket::bind(("0.0.0.0", 0)).context("socket d'envoi vidéo")?;
    let dest = SocketAddr::new(cfg.client, cfg.port);

    let shared = Arc::new(Shared::new());
    {
        let s = shared.clone();
        ctrlc::set_handler(move || s.stop.store(true, Relaxed)).context("gestionnaire Ctrl+C")?;
    }

    let mut child = ffmpeg::spawn(&cfg)?;
    let stdout = child.stdout.take().context("stdout de ffmpeg absent")?;
    let stderr = child.stderr.take().context("stderr de ffmpeg absent")?;

    let pacing = (!cfg.no_pacing).then(|| PacingParams {
        bitrate_bps: cfg.bitrate_mbps * 1e6,
        pacing_factor: cfg.pacing_factor,
        frame_interval: Duration::from_secs_f64(1.0 / f64::from(cfg.fps)),
    });
    log::info!(
        "orbit-sender → {dest} | {} {} i/s {} Mbit/s | FEC {} % (plancher {}) | pacing {} | \
         découpe {:?} | retour UDP {}",
        codec.name(),
        cfg.fps,
        cfg.bitrate_mbps,
        cfg.fec_percent,
        cfg.fec_min_parity,
        if pacing.is_some() {
            format!("×{} rafales de {}", cfg.pacing_factor, cfg.burst)
        } else {
            "désactivé".into()
        },
        cfg.split,
        cfg.feedback_port,
    );
    if !cfg.no_feedback_wait {
        log::info!("en attente du premier HELLO de {} (rien n'est envoyé avant)", cfg.client);
    }

    let (chunk_tx, chunk_rx) = crossbeam_channel::bounded(256);
    let (frame_tx, frame_rx) = crossbeam_channel::bounded(2);

    let lecteur = std::thread::Builder::new()
        .name("lecteur".into())
        .spawn(move || pipeline::run_reader(stdout, chunk_tx))?;
    let erreurs_ffmpeg =
        std::thread::Builder::new().name("stderr-ffmpeg".into()).spawn(move || {
            for ligne in BufReader::new(stderr).lines().map_while(Result::ok) {
                log::warn!("ffmpeg : {ligne}");
            }
        })?;
    let decoupeur = {
        let shared = shared.clone();
        let settings = SplitterSettings {
            codec,
            silence: (cfg.split == SplitMode::Silence)
                .then(|| Duration::from_micros(cfg.split_silence_us)),
            ignore_feedback: cfg.no_feedback_wait,
        };
        std::thread::Builder::new().name("decoupeur".into()).spawn(move || {
            pipeline::run_splitter(chunk_rx, frame_tx, fragmenter, settings, shared)
        })?
    };
    let envoi = {
        let shared = shared.clone();
        let settings = SenderSettings {
            dest,
            pacing,
            burst: cfg.burst as usize,
            simulate_loss_percent: cfg.simulate_loss,
            seed: cfg.seed,
        };
        std::thread::Builder::new()
            .name("envoi".into())
            .spawn(move || pipeline::run_sender(video_socket, frame_rx, settings, shared))?
    };
    let retour = {
        let shared = shared.clone();
        let client = cfg.client;
        std::thread::Builder::new()
            .name("retour".into())
            .spawn(move || feedback::run_feedback(feedback_socket, client, shared))?
    };

    // Boucle principale : résumé chaque seconde, surveillance de ffmpeg et de Ctrl+C.
    let mut resultat = Ok(());
    let mut dernier_resume = Instant::now();
    let fin = cfg.duration_s.map(|d| Instant::now() + Duration::from_secs_f64(d.max(0.0)));
    while !shared.stop.load(Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
        if fin.is_some_and(|f| Instant::now() >= f) {
            log::info!("durée --duration-s atteinte");
            break;
        }
        match child.try_wait() {
            Ok(Some(statut)) => {
                // Ctrl+C arrive aussi à ffmpeg (même console) : il peut s'arrêter juste avant
                // que notre propre drapeau soit levé.
                std::thread::sleep(Duration::from_millis(300));
                if !shared.stop.load(Relaxed) {
                    resultat = Err(anyhow::anyhow!("ffmpeg s'est arrêté ({statut})"));
                }
                break;
            }
            Ok(None) => {}
            Err(e) => {
                resultat = Err(anyhow::anyhow!("état de ffmpeg illisible : {e}"));
                break;
            }
        }
        let ecoule = dernier_resume.elapsed();
        if ecoule >= Duration::from_secs(1) {
            dernier_resume = Instant::now();
            let s = shared.counters.take();
            let etat = if cfg.no_feedback_wait {
                "sans-retour"
            } else if shared.feedback_alive() {
                "actif"
            } else {
                "pause"
            };
            let gigue = shared.gauges.fb_jitter_us.load(Relaxed);
            log::info!("{}", stats::summary_line(etat, &s, gigue, ecoule.as_secs_f64()));
        }
    }

    log::info!("arrêt…");
    shared.stop.store(true, Relaxed);
    feedback::wake_up(cfg.feedback_port);
    let _ = child.kill();
    let _ = child.wait();
    for (nom, t) in [
        ("lecteur", lecteur),
        ("découpeur", decoupeur),
        ("envoi", envoi),
        ("retour", retour),
        ("stderr", erreurs_ffmpeg),
    ] {
        if t.join().is_err() {
            log::error!("le thread {nom} a paniqué");
        }
    }
    resultat
}
