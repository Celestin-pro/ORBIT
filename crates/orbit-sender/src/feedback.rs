//! Thread du canal retour : écoute les paquets "OF" du portable (UDP 5003).
//!
//! Seule l'IP de `--client` est acceptée. En v1, une demande d'image clé ne peut pas être
//! transmise à ffmpeg : elle est comptée et journalisée (le flux garde une image clé périodique).

use std::net::{IpAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use orbit_protocol::feedback::{Feedback, decode};

use crate::pipeline::Shared;

/// Lecture **bloquante sans délai** : sous Windows, un `recv_from` qui expire (SO_RCVTIMEO) peut
/// perdre le datagramme qui arrive au même instant. Pour l'arrêt, le thread principal lève
/// `stop` puis s'envoie un datagramme local qui débloque la lecture ([`wake_up`]).
pub fn run_feedback(socket: UdpSocket, client: IpAddr, shared: Arc<Shared>) {
    let c = &shared.counters;
    let g = &shared.gauges;
    let mut buf = [0u8; 2048];
    while !shared.stop.load(Relaxed) {
        let (n, source) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            // ICMP « port injoignable » remonté par Windows : sans importance ici.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
            Err(e) => {
                log::warn!("canal retour : {e}");
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        if shared.stop.load(Relaxed) {
            break;
        }
        if source.ip() != client {
            c.add(&c.fb_rejected, 1);
            continue;
        }
        match decode(&buf[..n]) {
            Ok((_seq, message)) => {
                shared.note_feedback();
                match message {
                    Feedback::Hello => c.add(&c.fb_hello, 1),
                    Feedback::Stats(s) => {
                        c.add(&c.fb_stats, 1);
                        c.add(&c.fb_frames_ok, u64::from(s.frames_ok));
                        c.add(&c.fb_frames_fec, u64::from(s.frames_fec_recovered));
                        c.add(&c.fb_frames_lost, u64::from(s.frames_lost));
                        c.add(&c.fb_shards_received, u64::from(s.shards_received));
                        c.add(&c.fb_shards_lost, u64::from(s.shards_lost));
                        g.fb_jitter_us.store(u64::from(s.jitter_us), Relaxed);
                        g.fb_last_frame_id.store(u64::from(s.last_frame_id), Relaxed);
                    }
                    Feedback::KeyframeRequest { lost_frame_id } => {
                        c.add(&c.fb_keyframe_requests, 1);
                        log::info!(
                            "demande d'image clé du portable (image {lost_frame_id} perdue) : \
                             non transmissible à ffmpeg en v1, image clé périodique conservée"
                        );
                    }
                }
            }
            Err(e) => {
                c.add(&c.fb_rejected, 1);
                log::debug!("paquet retour rejeté : {e}");
            }
        }
    }
    log::debug!("canal retour : fin");
}

/// Débloque [`run_feedback`] (à appeler après avoir levé `stop`).
pub fn wake_up(port: u16) {
    if let Ok(s) = UdpSocket::bind(("127.0.0.1", 0)) {
        let _ = s.send_to(b"stop", ("127.0.0.1", port));
    }
}
