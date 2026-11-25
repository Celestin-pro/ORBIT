//! Tests de propriété et test de bout en bout du protocole OVP (émetteur → réseau simulé →
//! réassembleur).

use orbit_protocol::annexb::AnnexBSplitter;
use orbit_protocol::fragment::{FecParams, Fragmenter, FrameMeta};
use orbit_protocol::packet::VideoHeader;
use orbit_protocol::reassembly::{Event, LossPolicy, Reassembler, ReassemblerConfig};
use orbit_protocol::rng::SplitMix64;
use orbit_protocol::{Codec, packet};
use proptest::prelude::*;

fn fragmenter(percent: u8) -> Fragmenter {
    Fragmenter::new(FecParams { percent, min_parity: 2 }).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Une image découpée puis réassemblée sans perte redonne exactement les mêmes octets,
    /// quel que soit l'ordre d'arrivée des fragments.
    #[test]
    fn aller_retour_dans_le_desordre(
        taille in 1usize..300_000,
        percent in 0u8..=100,
        graine in any::<u64>(),
    ) {
        let data: Vec<u8> = { let mut r = SplitMix64::new(graine); (0..taille).map(|_| r.next_u64() as u8).collect() };
        let meta = FrameMeta { frame_id: 1, keyframe: true, codec: Codec::H264, send_ts_us: 0 };
        let mut ps = Vec::new();
        fragmenter(percent).fragment(&meta, &data, &mut ps).unwrap();
        // Mélange de Fisher-Yates.
        let mut r = SplitMix64::new(graine ^ 0xABCD);
        for i in (1..ps.len()).rev() {
            ps.swap(i, r.below(i as u64 + 1) as usize);
        }
        let mut re = Reassembler::new(ReassemblerConfig::default());
        for p in &ps {
            prop_assert!(p.len() <= packet::MAX_DATAGRAM_LEN);
            re.push(p, 0);
        }
        match re.poll(0) {
            Some(Event::Frame(f)) => prop_assert_eq!(f.data, data),
            autre => prop_assert!(false, "attendu une image, reçu {:?}", autre),
        }
    }

    /// Test strict de la FEC : au plus `parity_shards` fragments perdus par bloc → aucune image
    /// perdue.
    #[test]
    fn pertes_bornees_par_la_parite(
        taille in 1usize..250_000,
        percent in 1u8..=60,
        graine in any::<u64>(),
    ) {
        let mut r = SplitMix64::new(graine);
        let data: Vec<u8> = (0..taille).map(|_| r.next_u64() as u8).collect();
        let meta = FrameMeta { frame_id: 9, keyframe: true, codec: Codec::Hevc, send_ts_us: 0 };
        let mut ps = Vec::new();
        fragmenter(percent).fragment(&meta, &data, &mut ps).unwrap();
        // Regroupement par bloc puis suppression de `parity` fragments au hasard dans chacun.
        let mut garde = Vec::new();
        let mut i = 0;
        while i < ps.len() {
            let (h, _) = VideoHeader::parse(&ps[i]).unwrap();
            let total = h.data_shards as usize + h.parity_shards as usize;
            let mut bloc: Vec<Vec<u8>> = ps[i..i + total].to_vec();
            let a_perdre = r.below(h.parity_shards as u64 + 1) as usize;
            for _ in 0..a_perdre {
                let k = r.below(bloc.len() as u64) as usize;
                bloc.remove(k);
            }
            garde.extend(bloc);
            i += total;
        }
        let mut re = Reassembler::new(ReassemblerConfig::default());
        for p in &garde {
            re.push(p, 0);
        }
        match re.poll(0) {
            Some(Event::Frame(f)) => prop_assert_eq!(f.data, data),
            autre => prop_assert!(false, "attendu une image, reçu {:?}", autre),
        }
    }

    /// Des octets quelconques ne font jamais paniquer le réassembleur.
    #[test]
    fn octets_aleatoires_sans_panique(paquets in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..1300), 0..50)) {
        let mut re = Reassembler::new(ReassemblerConfig::default());
        for (t, p) in paquets.iter().enumerate() {
            re.push(p, t as u64 * 1000);
            while re.poll(t as u64 * 1000).is_some() {}
        }
    }

    /// En-têtes valides mais champs aléatoires : jamais de panique non plus.
    #[test]
    fn entetes_valides_aleatoires(champs in proptest::collection::vec((any::<u32>(), any::<u8>(), 1u8..=4, any::<u8>(), 1u8..=20, 0u8..=5, 1u16..=64, 1u32..2000, 1u32..4000), 0..80)) {
        let mut re = Reassembler::new(ReassemblerConfig::default());
        for (t, (id, bi, bc, si, d, p, ss, bl, fl)) in champs.into_iter().enumerate() {
            let total = d as u16 + p as u16;
            let h = VideoHeader {
                keyframe: id % 3 == 0,
                parity: u16::from(si) % total >= u16::from(d),
                frame_id: id % 16,
                block_index: bi % bc,
                block_count: bc,
                shard_index: (u16::from(si) % total) as u8,
                data_shards: d,
                parity_shards: p,
                codec: Codec::H264,
                shard_size: ss,
                block_len: bl.min(u32::from(d) * u32::from(ss)),
                frame_len: fl.max(bl),
                send_ts_us: 0,
            };
            re.push(&h.to_datagram(&vec![0; ss as usize]), t as u64 * 500);
            while re.poll(t as u64 * 500).is_some() {}
        }
    }

    /// Le découpeur Annex B retrouve les mêmes images quel que soit le découpage des lectures.
    #[test]
    fn annexb_independant_des_lectures(coupes in proptest::collection::vec(1usize..64, 1..200)) {
        let aud: &[u8] = &[0, 0, 0, 1, 0x09, 0xF0];
        let idr: &[u8] = &[0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 0x88, 0, 3, 9];
        let p: &[u8] = &[0, 0, 1, 0x41, 0x9A, 7, 0, 0, 3, 1, 8];
        let images: Vec<Vec<u8>> = (0..10).map(|i| if i % 4 == 0 { [aud, idr].concat() } else { [aud, p].concat() }).collect();
        let flux = images.concat();
        let mut s = AnnexBSplitter::new(Codec::H264);
        let mut out = Vec::new();
        let mut pos = 0;
        let mut k = 0;
        while pos < flux.len() {
            let n = coupes[k % coupes.len()].min(flux.len() - pos);
            s.push(&flux[pos..pos + n], &mut out);
            pos += n;
            k += 1;
        }
        out.extend(s.finish());
        prop_assert_eq!(out.into_iter().map(|u| u.data).collect::<Vec<_>>(), images);
    }
}

/// Test de bout en bout à graine fixe : 600 images de tailles réalistes (P de 30 à 60 Ko, une
/// clé de 200 Ko par seconde), 5 % de pertes indépendantes, FEC 20 % avec plancher de 2.
///
/// À 5 % de pertes, une image P de 35 + 7 fragments est perdue avec une probabilité d'environ
/// 0,1 % : « 0 image perdue » est donc vrai **pour cette graine**, pas une garantie. Si la
/// graine ou les tailles changent, une ou deux pertes restent statistiquement possibles.
#[test]
fn bout_en_bout_graine_fixe() {
    let mut r = SplitMix64::new(0x0EB1_7000);
    let mut frag = fragmenter(20);
    let mut re = Reassembler::new(ReassemblerConfig {
        policy: LossPolicy::Freeze,
        ..ReassemblerConfig::default()
    });
    let mut paquets = Vec::new();
    let mut livrees = 0;
    let mut envoyes = 0u64;
    let mut jetes = 0u64;
    for id in 0..600u32 {
        let cle = id % 60 == 0;
        let taille = if cle { 200_000 } else { 30_000 + r.below(30_001) as usize };
        let data: Vec<u8> = (0..taille).map(|i| (i as u32 ^ id) as u8).collect();
        let t = u64::from(id) * 16_667;
        let meta = FrameMeta { frame_id: id, keyframe: cle, codec: Codec::H264, send_ts_us: t };
        frag.fragment(&meta, &data, &mut paquets).unwrap();
        for (k, p) in paquets.iter().enumerate() {
            envoyes += 1;
            if r.chance_percent(5.0) {
                jetes += 1;
                continue;
            }
            re.push(p, t + k as u64 * 50);
        }
        while let Some(ev) = re.poll(t + 15_000) {
            match ev {
                Event::Frame(f) => {
                    assert_eq!(f.data, data, "image {id} corrompue");
                    livrees += 1;
                }
                Event::Lost { frame_id } => panic!("image {frame_id} perdue"),
            }
        }
    }
    let s = re.stats();
    assert_eq!(livrees, 600);
    assert_eq!(s.frames_lost, 0);
    assert!(s.frames_fec_recovered > 400, "{s:?}");
    assert!(jetes > envoyes / 25, "pertes simulées : {jetes} sur {envoyes}");
}
