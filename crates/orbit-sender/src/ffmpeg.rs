//! Construction de la commande ffmpeg (capture + encodage → H.264/HEVC Annex B sur stdout).
//!
//! La construction des arguments est pure et testée ; le lancement est dans [`spawn`].

use std::process::{Child, Command, Stdio};

use crate::config::{CodecArg, Config, EncoderArg, SourceArg};

/// Arguments passés à ffmpeg (sans le nom du programme).
pub fn build_args(c: &Config) -> anyhow::Result<Vec<String>> {
    let taille = c.scale_size()?;
    let mut a: Vec<String> = Vec::new();
    let mut push = |s: &str| a.push(s.to_string());

    push("-hide_banner");
    push("-loglevel");
    push("warning");
    push("-nostdin");

    match c.source {
        SourceArg::Desktop => {
            push("-f");
            push("lavfi");
            push("-i");
            push(&format!("ddagrab=framerate={}", c.fps));
            if let Some((l, h)) = taille {
                // Comme dans PROTOCOL.md : redescente en mémoire pour la mise à l'échelle.
                push("-vf");
                push(&format!("hwdownload,format=bgra,scale={l}:{h},format=nv12"));
            }
        }
        SourceArg::Test => {
            let (l, h) = taille.unwrap_or((1920, 1080));
            push("-re");
            push("-f");
            push("lavfi");
            push("-i");
            push(&format!("testsrc2=size={l}x{h}:rate={}", c.fps));
            if c.encoder != EncoderArg::Nvenc {
                // libopenh264 / MF / QSV n'acceptent pas toujours le yuv444 de testsrc2.
                push("-pix_fmt");
                push("yuv420p");
            }
        }
    }

    let hevc = c.codec == CodecArg::Hevc;
    let b = c.bitrate_mbps;
    let gop = ((f64::from(c.fps) * c.keyframe_interval_s).round() as u32).max(1);
    // Tampon de 2 images : le débit reste lisse image par image.
    let bufsize_k = ((b * 1000.0 * 2.0) / f64::from(c.fps.max(1))).round().max(1.0) as u64;

    let mut needs_aud_bsf = false;
    match c.encoder {
        EncoderArg::Nvenc => {
            push("-c:v");
            push(if hevc { "hevc_nvenc" } else { "h264_nvenc" });
            for s in ["-preset", "p1", "-tune", "ull", "-zerolatency", "1", "-delay", "0"] {
                push(s);
            }
            push("-rc");
            push("cbr");
            push("-forced-idr");
            push("1");
            push("-aud");
            push("1");
        }
        EncoderArg::Qsv => {
            push("-c:v");
            push(if hevc { "hevc_qsv" } else { "h264_qsv" });
            for s in ["-preset", "veryfast", "-async_depth", "1"] {
                push(s);
            }
            if !hevc {
                push("-look_ahead"); // option propre à h264_qsv
                push("0");
            }
            push("-aud");
            push("1");
        }
        EncoderArg::Mf => {
            push("-c:v");
            push(if hevc { "hevc_mf" } else { "h264_mf" });
            push("-hw_encoding");
            push("1");
            needs_aud_bsf = true;
        }
        EncoderArg::Openh264 => {
            anyhow::ensure!(!hevc, "libopenh264 n'encode que le H.264");
            push("-c:v");
            push("libopenh264");
            needs_aud_bsf = true;
        }
    }
    push("-bf");
    push("0");
    push("-b:v");
    push(&format!("{b}M"));
    push("-maxrate");
    push(&format!("{b}M"));
    push("-bufsize");
    push(&format!("{bufsize_k}k"));
    push("-g");
    push(&gop.to_string());
    if needs_aud_bsf {
        // Les délimiteurs d'unité d'accès servent au découpage (filtre LGPL).
        push("-bsf:v");
        push(if hevc { "hevc_metadata=aud=insert" } else { "h264_metadata=aud=insert" });
    }
    // Vider la sortie après chaque paquet : sinon le découpage sur silence ne voit rien.
    push("-flush_packets");
    push("1");
    push("-f");
    push(if hevc { "hevc" } else { "h264" });
    push("-");
    Ok(a)
}

/// Lance ffmpeg, stdout et stderr redirigés vers des tubes.
pub fn spawn(c: &Config) -> anyhow::Result<Child> {
    let args = build_args(c)?;
    log::info!("ffmpeg {}", args.join(" "));
    Command::new(&c.ffmpeg)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("impossible de lancer ffmpeg ({}) : {e}", c.ffmpeg.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn cfg(extra: &[&str]) -> Config {
        let mut v = vec!["orbit-sender", "--client", "127.0.0.1"];
        v.extend_from_slice(extra);
        Config::parse_from(v)
    }

    #[test]
    fn bureau_nvenc_h264() {
        let a = build_args(&cfg(&[])).unwrap().join(" ");
        assert!(a.contains("-f lavfi -i ddagrab=framerate=60"));
        assert!(a.contains("-vf hwdownload,format=bgra,scale=1920:1080,format=nv12"));
        assert!(a.contains("-c:v h264_nvenc -preset p1 -tune ull -zerolatency 1 -delay 0"));
        assert!(a.contains("-aud 1"));
        assert!(a.contains("-b:v 20M -maxrate 20M -bufsize 667k -g 60"));
        assert!(a.ends_with("-flush_packets 1 -f h264 -"));
        assert!(!a.contains("-bsf"));
    }

    #[test]
    fn bureau_natif_hevc() {
        let a = build_args(&cfg(&["--codec", "hevc", "--scale", "native"])).unwrap().join(" ");
        assert!(!a.contains("-vf"));
        assert!(a.contains("hevc_nvenc"));
        assert!(a.ends_with("-f hevc -"));
    }

    #[test]
    fn mire_sans_aud_natif() {
        let a = build_args(&cfg(&["--source", "test", "--encoder", "openh264"])).unwrap().join(" ");
        assert!(a.contains("-re -f lavfi -i testsrc2=size=1920x1080:rate=60"));
        assert!(a.contains("-bsf:v h264_metadata=aud=insert"));
        assert!(build_args(&cfg(&["--encoder", "openh264", "--codec", "hevc"])).is_err());
    }
}
