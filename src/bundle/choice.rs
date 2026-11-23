//! Which bundle to inject from, when there is more than one to pick.
//!
//! The client file lists the bundles **on offer** (`bundles = [...]`) and points at
//! one of them as the **default** (`default_bundle =`; without it the default is the
//! packed config). They are what a person is offered; nothing here asks, or reads a file, or knows what a
//! reference means. That is the command layer's, which asks and writes, and the
//! stores', which read.

use crate::config::client::Client;

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
pub fn default(settings: &Client) -> Offer {
    settings.default_bundle.as_deref().into()
}

/// What `up` offers: the `default` first, then the rest of the `listed` bundles
/// (the default is one of them, when it is a reference), each once.
pub fn options(default: &Offer, listed: &[String]) -> Vec<Offer> {
    let mut all = vec![default.clone()];
    for reference in listed {
        let candidate = Offer::Reference(reference.clone());
        if !all.contains(&candidate) {
            all.push(candidate);
        }
    }
    all
}

/// What `ldg bundle use` offers: [`options`], and the packed config where this
/// machine can pack it, even when a bundle is the default.
pub fn selectable(default: &Offer, listed: &[String], can_pack: bool) -> Vec<Offer> {
    let mut all = options(default, listed);
    if can_pack && !all.contains(&Offer::Live) {
        all.push(Offer::Live);
    }
    all
}

/// The references to list once `new` is the default: the default is one of the
/// listed bundles, so a reference that is not there yet is added; the packed config
/// is not a reference and adds nothing. What was the default stays listed.
pub fn with_default(listed: &[String], new: &Offer) -> Vec<String> {
    let mut list = listed.to_vec();
    if let Some(reference) = new.reference()
        && !list.iter().any(|r| r == reference)
    {
        list.push(reference.to_string());
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::client::Client;

    fn config(bundle: Option<&str>, bundles: &[&str]) -> Client {
        Client {
            default_bundle: bundle.map(Into::into),
            bundles: bundles.iter().map(|b| b.to_string()).collect(),
            ..Client::default()
        }
    }

    fn options(c: &Client) -> Vec<Offer> {
        super::options(&default(c), &c.bundles)
    }

    fn selectable(c: &Client, can_pack: bool) -> Vec<Offer> {
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
    fn making_a_bundle_the_default_lists_it_and_keeps_what_was_listed() {
        let listed = vec!["a.tar".to_string(), "c.tar".to_string()];
        assert_eq!(
            with_default(&listed, &reference("b.tar")),
            vec![
                "a.tar".to_string(),
                "c.tar".to_string(),
                "b.tar".to_string()
            ]
        );
    }

    #[test]
    fn a_default_that_is_listed_already_changes_nothing_and_the_packed_config_lists_nothing() {
        let listed = vec!["a.tar".to_string(), "b.tar".to_string()];
        assert_eq!(with_default(&listed, &reference("b.tar")), listed);
        assert_eq!(with_default(&listed, &Offer::Live), listed);
        assert_eq!(with_default(&[], &Offer::Live), Vec::<String>::new());
    }
}
