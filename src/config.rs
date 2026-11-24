//! Arguments de la ligne de commande : contrat avec `bridge.py` (voir CLAUDE.md).
//!
//! Tous les arguments de la v0.1 sont déclarés dès maintenant, même ceux que les étapes
//! suivantes exploiteront (`--no-input`, `--audio-buffer-ms`...), pour que le contrat avec
//! le pont ne change plus.

use std::fmt;
use std::net::IpAddr;

use clap::{Parser, ValueEnum};

/// Transport du flux vidéo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum Transport {
    /// MPEG-TS sur UDP brut.
    Udp,
    /// MPEG-TS sur SRT ; le portable écoute (`mode=listener`), le PC fixe appelle.
    Srt,
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Transport::Udp => f.write_str("udp"),
            Transport::Srt => f.write_str("srt"),
        }
    }
}

/// Synchronisation verticale à la présentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum Vsync {
    /// `Present(1, 0)` : une image par rafraîchissement d'écran.
    On,
    /// `Present(0, ALLOW_TEARING)` : on présente dès qu'une image arrive (défaut, latence mini).
    Off,
}

impl Vsync {
    /// Vrai si la synchronisation verticale est demandée.
    pub fn est_active(self) -> bool {
        matches!(self, Vsync::On)
    }
}

impl fmt::Display for Vsync {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Vsync::On => f.write_str("on"),
            Vsync::Off => f.write_str("off"),
        }
    }
}

/// Configuration complète du player.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "orbit-player",
    version,
    about = "Récepteur de streaming d'ORBIT : vidéo, entrées et son en une seule fenêtre",
    long_about = None
)]
pub struct Config {
    /// Adresse IP du PC fixe (hôte) : destination des entrées, et seule source de son acceptée.
    #[arg(long, value_name = "IP")]
    pub host: IpAddr,

    /// Transport du flux vidéo.
    #[arg(long, default_value_t = Transport::Udp)]
    pub transport: Transport,

    /// Port d'écoute de la vidéo.
    #[arg(long, default_value_t = 5000, value_name = "PORT")]
    pub video_port: u16,

    /// Port de destination des entrées (sur l'hôte).
    #[arg(long, default_value_t = 5001, value_name = "PORT")]
    pub input_port: u16,

    /// Port d'écoute du son.
    #[arg(long, default_value_t = 5002, value_name = "PORT")]
    pub audio_port: u16,

    /// Latence SRT demandée, en millisecondes (ignorée en UDP).
    #[arg(long, default_value_t = 40, value_name = "MS")]
    pub srt_latency_ms: u32,

    /// Démarrer en plein écran.
    #[arg(long)]
    pub fullscreen: bool,

    /// Synchronisation verticale.
    #[arg(long, default_value_t = Vsync::Off)]
    pub vsync: Vsync,

    /// Ne pas capturer ni envoyer les entrées.
    #[arg(long)]
    pub no_input: bool,

    /// Ne pas recevoir ni jouer le son.
    #[arg(long)]
    pub no_audio: bool,

    /// Taille cible du tampon audio anti-gigue, en millisecondes.
    #[arg(long, default_value_t = 30, value_name = "MS")]
    pub audio_buffer_ms: u32,

    /// Titre de la fenêtre.
    #[arg(long, default_value = "ORBIT", value_name = "TEXTE")]
    pub title: String,
}

impl Config {
    /// URL d'entrée pour libavformat, options basse latence incluses.
    ///
    /// UDP : on écoute sur toutes les interfaces, avec un gros tampon noyau pour absorber
    /// les rafales d'un flux 1080p60.
    /// SRT : **le portable écoute** et le PC fixe appelle (`mode=listener`).
    pub fn url_video(&self) -> String {
        match self.transport {
            Transport::Udp => format!(
                "udp://0.0.0.0:{port}?fifo_size=5000000&overrun_nonfatal=1&buffer_size=8388608",
                port = self.video_port
            ),
            Transport::Srt => format!(
                "srt://0.0.0.0:{port}?mode=listener&latency={latence}&transtype=live",
                port = self.video_port,
                // SRT exprime sa latence en microsecondes.
                latence = u64::from(self.srt_latency_ms) * 1_000
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn config_minimale() -> Config {
        Config::parse_from(["orbit-player", "--host", "192.168.1.10"])
    }

    #[test]
    fn la_definition_clap_est_coherente() {
        Config::command().debug_assert();
    }

    #[test]
    fn les_defauts_respectent_le_contrat_du_pont() {
        let config = config_minimale();
        assert_eq!(config.transport, Transport::Udp);
        assert_eq!(config.video_port, 5000);
        assert_eq!(config.input_port, 5001);
        assert_eq!(config.audio_port, 5002);
        assert_eq!(config.srt_latency_ms, 40);
        assert_eq!(config.vsync, Vsync::Off);
        assert_eq!(config.audio_buffer_ms, 30);
        assert!(!config.fullscreen);
        assert!(!config.no_input);
        assert!(!config.no_audio);
    }

    #[test]
    fn url_udp_contient_les_options_basse_latence() {
        let url = config_minimale().url_video();
        assert!(url.starts_with("udp://0.0.0.0:5000?"), "url = {url}");
        assert!(url.contains("fifo_size=5000000"));
        assert!(url.contains("overrun_nonfatal=1"));
        assert!(url.contains("buffer_size=8388608"));
    }

    #[test]
    fn url_srt_ecoute_et_convertit_la_latence_en_microsecondes() {
        let config = Config::parse_from([
            "orbit-player",
            "--host",
            "192.168.1.10",
            "--transport",
            "srt",
            "--srt-latency-ms",
            "40",
        ]);
        let url = config.url_video();
        assert!(url.starts_with("srt://0.0.0.0:5000?"), "url = {url}");
        assert!(url.contains("mode=listener"));
        assert!(url.contains("latency=40000"));
        assert!(url.contains("transtype=live"));
    }

    #[test]
    fn host_invalide_est_refuse() {
        assert!(Config::try_parse_from(["orbit-player", "--host", "pas-une-ip"]).is_err());
    }
}
