//! Arguments de la ligne de commande (PROTOCOL.md, « orbit-sender »).

use std::net::IpAddr;
use std::path::PathBuf;

use clap::{Parser, ValueEnum};

/// Émetteur vidéo OVP : capture + encodage par ffmpeg, puis découpage, FEC et pacing.
#[derive(Debug, Clone, Parser)]
#[command(name = "orbit-sender", version)]
pub struct Config {
    /// IP du PC portable (destination de la vidéo ; seule IP acceptée sur le port retour).
    #[arg(long)]
    pub client: IpAddr,

    /// Port vidéo du portable.
    #[arg(long, default_value_t = 5000)]
    pub port: u16,

    /// Port local du canal retour (paquets "OF").
    #[arg(long, default_value_t = 5003)]
    pub feedback_port: u16,

    #[arg(long, value_enum, default_value_t = CodecArg::H264)]
    pub codec: CodecArg,

    #[arg(long, default_value_t = 60)]
    pub fps: u32,

    #[arg(long, default_value_t = 20.0)]
    pub bitrate_mbps: f64,

    /// Résolution de sortie : `LxH` (ex. 1920x1080) ou `native`.
    #[arg(long, default_value = "1920x1080")]
    pub scale: String,

    /// Intervalle entre images clés, en secondes.
    #[arg(long, default_value_t = 1.0)]
    pub keyframe_interval_s: f64,

    /// Pourcentage de parité FEC (0 = aucune parité).
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u8).range(0..=100))]
    pub fec_percent: u8,

    /// Plancher de fragments de parité par bloc quand --fec-percent > 0.
    #[arg(long, default_value_t = 2)]
    pub fec_min_parity: u8,

    /// Débit de pacing = facteur × débit vidéo (au minimum de quoi tenir 75 % d'un intervalle).
    #[arg(long, default_value_t = 1.5)]
    pub pacing_factor: f64,

    /// Paquets par micro-rafale.
    #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u32).range(1..=64))]
    pub burst: u32,

    /// Envoi en rafale, sans pacing (pour comparer).
    #[arg(long)]
    pub no_pacing: bool,

    /// Découpage des images : `silence` (dès que ffmpeg se tait) ou `aud` (attendre l'AUD
    /// suivant, une image de latence en plus).
    #[arg(long, value_enum, default_value_t = SplitMode::Silence)]
    pub split: SplitMode,

    /// Durée de silence de ffmpeg qui termine une image (mode `silence`), en µs.
    #[arg(long, default_value_t = 1000)]
    pub split_silence_us: u64,

    #[arg(long, value_enum, default_value_t = SourceArg::Desktop)]
    pub source: SourceArg,

    /// Encodeur ffmpeg. `nvenc` sur le PC fixe ; `qsv`, `mf` ou `openh264` pour développer
    /// sur une machine sans NVIDIA (avec --source test).
    #[arg(long, value_enum, default_value_t = EncoderArg::Nvenc)]
    pub encoder: EncoderArg,

    /// Chemin de ffmpeg (sinon : celui du PATH).
    #[arg(long, default_value = "ffmpeg")]
    pub ffmpeg: PathBuf,

    /// Jette volontairement ce pourcentage de paquets à l'envoi (tests FEC).
    #[arg(long, default_value_t = 0.0)]
    pub simulate_loss: f64,

    /// Graine du tirage de --simulate-loss (reproductible).
    #[arg(long, default_value_t = 1)]
    pub seed: u64,

    /// Envoyer sans attendre le premier HELLO du portable (tests sans canal retour).
    #[arg(long)]
    pub no_feedback_wait: bool,

    /// Arrêt automatique après cette durée, en secondes (mesures comparatives, tests).
    #[arg(long)]
    pub duration_s: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CodecArg {
    H264,
    Hevc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SplitMode {
    Silence,
    Aud,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SourceArg {
    /// Capture de l'écran (ddagrab).
    Desktop,
    /// Mire testsrc2, pour développer sans capture.
    Test,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EncoderArg {
    Nvenc,
    Qsv,
    Mf,
    Openh264,
}

impl Config {
    /// Résolution demandée, `None` pour `native`.
    pub fn scale_size(&self) -> anyhow::Result<Option<(u32, u32)>> {
        parse_scale(&self.scale)
    }
}

pub fn parse_scale(s: &str) -> anyhow::Result<Option<(u32, u32)>> {
    if s.eq_ignore_ascii_case("native") {
        return Ok(None);
    }
    let (l, h) = s
        .split_once(['x', 'X'])
        .ok_or_else(|| anyhow::anyhow!("--scale attend LxH ou native, reçu « {s} »"))?;
    let l: u32 = l.trim().parse()?;
    let h: u32 = h.trim().parse()?;
    anyhow::ensure!(
        l >= 16 && h >= 16 && l % 2 == 0 && h % 2 == 0,
        "--scale : dimensions paires ≥ 16"
    );
    Ok(Some((l, h)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echelle() {
        assert_eq!(parse_scale("1920x1080").unwrap(), Some((1920, 1080)));
        assert_eq!(parse_scale("native").unwrap(), None);
        assert!(parse_scale("1920").is_err());
        assert!(parse_scale("1921x1080").is_err());
    }

    #[test]
    fn valeurs_par_defaut() {
        let c = Config::parse_from(["orbit-sender", "--client", "192.168.1.72"]);
        assert_eq!(c.port, 5000);
        assert_eq!(c.feedback_port, 5003);
        assert_eq!(c.fec_percent, 20);
        assert_eq!(c.fec_min_parity, 2);
        assert_eq!(c.split, SplitMode::Silence);
        assert!(!c.no_pacing);
    }
}
