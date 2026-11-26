//! Comparaisons de compteurs u32 qui rebouclent (`frame_id`, `seq`), modulo 2^32.

/// Écart signé `a - b` modulo 2^32 : positif si `a` est plus récent que `b`.
///
/// Valable tant que les deux valeurs sont à moins de 2^31 l'une de l'autre, ce qui est
/// toujours le cas pour des images en cours de réassemblage.
pub fn diff(a: u32, b: u32) -> i32 {
    a.wrapping_sub(b) as i32
}

/// Vrai si `a` est strictement plus récent que `b`.
pub fn is_newer(a: u32, b: u32) -> bool {
    diff(a, b) > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecart_simple() {
        assert_eq!(diff(10, 7), 3);
        assert_eq!(diff(7, 10), -3);
        assert!(is_newer(8, 7));
        assert!(!is_newer(7, 7));
    }

    #[test]
    fn ecart_au_rebouclage() {
        assert_eq!(diff(2, u32::MAX), 3);
        assert_eq!(diff(u32::MAX, 2), -3);
        assert!(is_newer(0, u32::MAX));
    }
}
