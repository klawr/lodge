//! Which bundle to inject from, when there is more than one to pick.
//!
//! The config names a **default** (`bundle =`, or the packed config when it is not
//! set) and may list **others** (`bundles = [...]`). Together they are what a
//! person is offered; nothing here asks, or reads a file, or knows what a
//! reference means. That is the command layer's, which asks and writes, and the
//! stores', which read.

use crate::config::Config;

/// One way to get a bundle: a reference, or the config packed as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offer {
    /// Pack the `[[binary]]` and `[[data]]` entries (Linux only).
    Live,
    Reference(String),
}

impl Offer {
    /// What the candidate is called when it is listed.
    pub fn label(&self) -> &str {
        match self {
            Offer::Live => "(pack the config)",
            Offer::Reference(r) => r,
        }
    }

    /// The reference, none for the packed config.
    pub fn reference(&self) -> Option<&str> {
        match self {
            Offer::Live => None,
            Offer::Reference(r) => Some(r),
        }
    }
}

impl From<Option<&str>> for Offer {
    fn from(reference: Option<&str>) -> Self {
        reference.map_or(Offer::Live, |r| Offer::Reference(r.to_string()))
    }
}

/// The bundle used when nothing says otherwise.
pub fn default(config: &Config) -> Offer {
    config.bundle.as_deref().into()
}

/// What `up` offers: the `default` first, then the listed `others`, each once.
pub fn options(default: &Offer, others: &[String]) -> Vec<Offer> {
    let mut all = vec![default.clone()];
    for reference in others {
        let candidate = Offer::Reference(reference.clone());
        if !all.contains(&candidate) {
            all.push(candidate);
        }
    }
    all
}

/// What `ldg bundle use` offers: [`options`], and the packed config where this
/// machine can pack it, even when a bundle is the default.
pub fn selectable(default: &Offer, others: &[String], can_pack: bool) -> Vec<Offer> {
    let mut all = options(default, others);
    if can_pack && !all.contains(&Offer::Live) {
        all.push(Offer::Live);
    }
    all
}

/// The references to list after the default changes from `old` to `new`: the old
/// default stays selectable, the new one is the default and so no longer an
/// "other".
pub fn after_switch(others: &[String], old: &Offer, new: &Offer) -> Vec<String> {
    let mut list: Vec<String> = others
        .iter()
        .filter(|r| Some(r.as_str()) != new.reference())
        .cloned()
        .collect();
    if let Some(old) = old.reference()
        && old != new.reference().unwrap_or_default()
        && !list.iter().any(|r| r == old)
    {
        list.push(old.to_string());
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(bundle: Option<&str>, bundles: &[&str]) -> Config {
        Config {
            bundle: bundle.map(Into::into),
            bundles: bundles.iter().map(|b| b.to_string()).collect(),
            ..Config::default()
        }
    }

    fn options(c: &Config) -> Vec<Offer> {
        super::options(&default(c), &c.bundles)
    }

    fn selectable(c: &Config, can_pack: bool) -> Vec<Offer> {
        super::selectable(&default(c), &c.bundles, can_pack)
    }

    fn reference(r: &str) -> Offer {
        Offer::Reference(r.into())
    }

    #[test]
    fn the_packed_config_is_the_default_until_a_bundle_is_set() {
        assert_eq!(default(&config(None, &[])), Offer::Live);
        assert_eq!(default(&config(Some("a.tar"), &[])), reference("a.tar"));
    }

    #[test]
    fn nothing_listed_is_one_option_so_nothing_is_asked() {
        assert_eq!(options(&config(Some("a.tar"), &[])).len(), 1);
        assert_eq!(options(&config(None, &[])).len(), 1);
    }

    #[test]
    fn the_default_comes_first_and_is_not_offered_twice() {
        let c = config(Some("a.tar"), &["b.tar", "a.tar", "b.tar"]);
        assert_eq!(options(&c), vec![reference("a.tar"), reference("b.tar")]);
    }

    #[test]
    fn without_a_default_bundle_the_packed_config_is_offered_with_the_others() {
        let c = config(None, &["b.tar"]);
        assert_eq!(options(&c), vec![Offer::Live, reference("b.tar")]);
    }

    #[test]
    fn the_packed_config_can_be_chosen_over_a_bundle_only_where_it_can_be_packed() {
        let c = config(Some("a.tar"), &[]);
        assert_eq!(selectable(&c, false), vec![reference("a.tar")]);
        assert_eq!(selectable(&c, true), vec![reference("a.tar"), Offer::Live]);
    }

    #[test]
    fn switching_keeps_the_old_default_selectable_and_drops_the_new_one_from_the_others() {
        let others = vec!["b.tar".to_string(), "c.tar".to_string()];
        assert_eq!(
            after_switch(&others, &reference("a.tar"), &reference("b.tar")),
            vec!["c.tar".to_string(), "a.tar".to_string()]
        );
    }

    #[test]
    fn switching_from_the_packed_config_and_to_it_lists_only_references() {
        let others: Vec<String> = vec![];
        assert_eq!(
            after_switch(&others, &Offer::Live, &reference("b.tar")),
            Vec::<String>::new()
        );
        assert_eq!(
            after_switch(&others, &reference("a.tar"), &Offer::Live),
            vec!["a.tar".to_string()]
        );
    }

    #[test]
    fn switching_to_the_same_default_changes_nothing() {
        let others = vec!["b.tar".to_string()];
        assert_eq!(
            after_switch(&others, &reference("a.tar"), &reference("a.tar")),
            others
        );
    }
}
