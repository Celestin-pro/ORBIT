//! Petit générateur pseudo-aléatoire à graine (SplitMix64).
//!
//! Sert à `--simulate-loss` (pertes volontaires, reproductibles) et aux tests de bout en bout à
//! graine fixe. Ce n'est **pas** un générateur cryptographique.

/// Générateur SplitMix64 (Steele, Lea, Flood 2014) : 64 bits d'état, rapide, bonne qualité
/// statistique pour des simulations.
#[derive(Debug, Clone)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        SplitMix64 { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Nombre uniforme dans [0, 1).
    pub fn next_f64(&mut self) -> f64 {
        // 53 bits de mantisse.
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Vrai avec la probabilité `percent` / 100.
    pub fn chance_percent(&mut self, percent: f64) -> bool {
        self.next_f64() * 100.0 < percent
    }

    /// Entier uniforme dans [0, n) (n > 0 ; renvoie 0 si n == 0).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reproductible() {
        let mut a = SplitMix64::new(42);
        let mut b = SplitMix64::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn proportion_approximative() {
        let mut r = SplitMix64::new(1);
        let n = 100_000;
        let touches = (0..n).filter(|_| r.chance_percent(5.0)).count();
        // 5 % ± 0,5 %
        assert!((4500..5500).contains(&touches), "{touches}");
    }
}
