//! Herdr agent names. `agent start` names must be unique among live agents and
//! match `[a-z][a-z0-9_-]{0,31}` (32 characters), so slugs are truncated.

#[cfg_attr(not(test), allow(dead_code))]
pub const MAX_AGENT_NAME: usize = 32;

fn head(slug: &str, n: usize) -> String {
    let mut s: String = slug.chars().take(n).collect();
    while s.ends_with('-') {
        s.pop();
    }
    s
}

/// `hpc-<slug truncated to 24>` for the first coordinator of a project,
/// `hpc-<slug truncated to 22>-N` for later ones (`n` ≥ 1).
pub fn coordinator(slug: &str, n: u32) -> String {
    if n == 0 {
        format!("hpc-{}", head(slug, 24))
    } else {
        format!("hpc-{}-{n}", head(slug, 22))
    }
}

/// Whether `name` is a coordinator name of THIS slug: `hpc-<head24>` or
/// `hpc-<head22>-N` (N ≥ 1). Checked structurally, not by prefix — the two
/// truncation widths and the trailing-hyphen trim are what make a prefix
/// match unsafe across slugs.
pub fn is_coordinator_name(slug: &str, name: &str) -> bool {
    if name == coordinator(slug, 0) {
        return true;
    }
    let prefix = format!("hpc-{}-", head(slug, 22));
    name.strip_prefix(&prefix).is_some_and(|suffix| suffix.parse::<u32>().is_ok_and(|n| n >= 1))
}

/// `hp-<slug truncated to 21>-<id>` (`t-0009` is 6 characters).
pub fn thread(slug: &str, id: &str) -> String {
    format!("hp-{}-{id}", head(slug, 21))
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn is_valid(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
        && name.len() <= MAX_AGENT_NAME
}

/// The first coordinator name that is not among `taken`.
pub fn free_coordinator(slug: &str, taken: &[String]) -> String {
    (0..).map(|n| coordinator(slug, n)).find(|name| !taken.contains(name)).unwrap_or_default()
}

/// Two slugs whose thread or coordinator names collide after truncation.
pub fn collide(a: &str, b: &str) -> bool {
    a != b && (coordinator(a, 0) == coordinator(b, 0) || thread(a, "t-0001") == thread(b, "t-0001"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_fit_herdrs_limit_for_the_longest_slug() {
        let slug = "a".repeat(40);
        for name in [coordinator(&slug, 0), coordinator(&slug, 99), thread(&slug, "t-12345")] {
            assert!(is_valid(&name), "{name} ({})", name.len());
        }
        assert_eq!(coordinator("demo", 0), "hpc-demo");
        assert_eq!(coordinator("demo", 2), "hpc-demo-2");
        assert_eq!(thread("demo", "t-0001"), "hp-demo-t-0001");
        // A truncation never leaves a trailing hyphen before the suffix.
        assert_eq!(coordinator("aaaaaaaaaaaaaaaaaaaaaaa-bbbb", 0), "hpc-aaaaaaaaaaaaaaaaaaaaaaa");
        assert!(!is_valid("Hp-x"));
        assert!(!is_valid(""));
    }

    #[test]
    fn free_names_skip_taken_ones_and_collisions_are_detected() {
        let taken = vec!["hpc-demo".to_string(), "hpc-demo-1".to_string()];
        assert_eq!(free_coordinator("demo", &taken), "hpc-demo-2");
        assert_eq!(free_coordinator("demo", &[]), "hpc-demo");
        let long = "x".repeat(30);
        assert!(collide(&format!("{long}-a"), &format!("{long}-b")));
        assert!(!collide("alpha", "beta"));
        assert!(!collide("alpha", "alpha"));
    }

    #[test]
    fn coordinator_name_family_is_decided_through_reconstructed_names() {
        assert!(is_coordinator_name("demo", "hpc-demo"));
        assert!(is_coordinator_name("demo", "hpc-demo-1"));
        assert!(is_coordinator_name("demo", "hpc-demo-42"));
        // N starts at 1; -0 is never allocated.
        assert!(!is_coordinator_name("demo", "hpc-demo-0"));
        assert!(!is_coordinator_name("demo", ""));
        assert!(!is_coordinator_name("demo", "hp-demo-t-0001"));
        assert!(!is_coordinator_name("demo", "hpc-other"));
        assert!(!is_coordinator_name("demo", "hpc-demo-extra"));
        assert!(!is_coordinator_name("demo", "Hpc-demo"));
        assert!(!is_coordinator_name("demo", "hpc-demo-1x"));
        // The 24/22 truncation widths: reconstruct, never prefix-match.
        let slug = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"; // 31 a's
        assert!(is_coordinator_name(&slug, &coordinator(&slug, 0)));
        assert!(is_coordinator_name(&slug, &coordinator(&slug, 7)));
        // One char shorter than the truncated head is a different (shorter) slug.
        assert!(!is_coordinator_name(&slug, "hpc-aaaaaaaaaaaaaaaaaaaaaaa"));
        // Two slugs whose 24-heads differ but share a shorter prefix.
        let longer = "demo-lation-system-project";
        assert!(!is_coordinator_name("demo", &coordinator(longer, 0)));
        assert!(!is_coordinator_name(longer, &coordinator("demo", 0)));
    }
}
