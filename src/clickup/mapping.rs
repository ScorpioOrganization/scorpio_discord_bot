//! Matching ClickUp people to Discord members.
//!
//! Explicit mappings always win. Everyone else is matched automatically by name: after
//! folding accents and punctuation the names must be equal, contain the same words, or be
//! a near typo of each other. A match must be clearly better than the runner-up and no
//! member is given to two people, otherwise the person stays unmatched ("ambiguous").

use std::collections::{BTreeSet, HashMap, HashSet};

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use super::client::User;
use crate::discord::members::MemberInfo;

/// How much better the best candidate must score than the next one.
const MARGIN: f64 = 0.03;
/// Normalized Levenshtein similarity needed for a typo level match.
const MIN_SIMILARITY: f64 = 0.85;
const MIN_KEY_LEN: usize = 3;
const MIN_SIMILAR_LEN: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    Name,
    Email,
    Words,
    Contains,
    Similar,
}

impl MatchKind {
    pub fn label(self) -> &'static str {
        match self {
            MatchKind::Name => "same name",
            MatchKind::Email => "email matches name",
            MatchKind::Words => "same words",
            MatchKind::Contains => "name contains",
            MatchKind::Similar => "similar name",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Explicit {
    Member(u64),
    /// Never map this person, not even automatically.
    Never,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    Explicit(u64),
    Auto {
        member: u64,
        kind: MatchKind,
    },
    Never,
    /// Several members fit equally well. Holds their ids.
    Ambiguous(Vec<u64>),
    Unmatched,
}

impl Resolution {
    pub fn member(&self) -> Option<u64> {
        match self {
            Resolution::Explicit(id) | Resolution::Auto { member: id, .. } => Some(*id),
            _ => None,
        }
    }
}

/// Lowercase with accents removed and letters like `ł` or `ß` spelled in ASCII.
pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.nfkd().filter(|c| !is_combining_mark(*c)) {
        match c {
            'ł' | 'Ł' => out.push('l'),
            'ø' | 'Ø' => out.push('o'),
            'đ' | 'Đ' | 'ð' | 'Ð' => out.push('d'),
            'ß' => out.push_str("ss"),
            'æ' | 'Æ' => out.push_str("ae"),
            'œ' | 'Œ' => out.push_str("oe"),
            'ħ' | 'Ħ' => out.push('h'),
            'ı' => out.push('i'),
            'þ' | 'Þ' => out.push_str("th"),
            c => out.extend(c.to_lowercase()),
        }
    }
    out
}

/// [`fold`] with everything but letters and digits removed: `Igor.Zawórski` -> `igorzaworski`.
pub fn normalize(s: &str) -> String {
    fold(s).chars().filter(|c| c.is_alphanumeric()).collect()
}

/// Words of a name after [`fold`], split on anything that is not a letter or digit.
pub fn words(s: &str) -> BTreeSet<String> {
    fold(s)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

struct Key {
    norm: String,
    words: BTreeSet<String>,
    email: bool,
}

impl Key {
    fn new(raw: &str, email: bool) -> Option<Self> {
        let norm = normalize(raw);
        (norm.chars().count() >= MIN_KEY_LEN).then(|| Self {
            norm,
            words: words(raw),
            email,
        })
    }
}

fn person_keys(p: &User) -> Vec<Key> {
    let local = p.email.split('@').next().unwrap_or_default();
    [Key::new(&p.username, false), Key::new(local, true)]
        .into_iter()
        .flatten()
        .collect()
}

fn member_keys(m: &MemberInfo) -> Vec<Key> {
    [
        m.nick.as_deref(),
        m.global_name.as_deref(),
        Some(&m.username),
    ]
    .into_iter()
    .flatten()
    .filter_map(|n| Key::new(n, false))
    .collect()
}

fn pair_score(a: &Key, b: &Key) -> Option<(f64, MatchKind)> {
    if a.norm == b.norm {
        return Some(if a.email {
            (0.99, MatchKind::Email)
        } else {
            (1.0, MatchKind::Name)
        });
    }
    if a.words.len() >= 2 && a.words == b.words {
        return Some((0.95, MatchKind::Words));
    }
    let common = a.words.intersection(&b.words).count();
    if common >= 2 && (common == a.words.len() || common == b.words.len()) {
        return Some((0.9, MatchKind::Contains));
    }
    if a.norm.chars().count() >= MIN_SIMILAR_LEN && b.norm.chars().count() >= MIN_SIMILAR_LEN {
        let sim = strsim::normalized_levenshtein(&a.norm, &b.norm);
        if sim >= MIN_SIMILARITY {
            return Some((0.7 + 0.2 * sim, MatchKind::Similar));
        }
    }
    None
}

fn score(person: &[Key], member: &[Key]) -> Option<(f64, MatchKind)> {
    person
        .iter()
        .flat_map(|a| member.iter().filter_map(move |b| pair_score(a, b)))
        .max_by(|x, y| x.0.total_cmp(&y.0))
}

/// Best member for a person among `pool`, or why there is none.
fn pick<'a>(person: &User, pool: impl Iterator<Item = &'a (MemberInfo, Vec<Key>)>) -> Pick {
    let keys = person_keys(person);
    let mut scored: Vec<(f64, MatchKind, u64)> = pool
        .filter_map(|(m, mk)| score(&keys, mk).map(|(s, k)| (s, k, m.id)))
        .collect();
    scored.sort_by(|x, y| y.0.total_cmp(&x.0));
    match scored.as_slice() {
        [] => Pick::None,
        [(s, k, id)] => Pick::One(*s, *k, *id),
        [(s, k, id), (next, ..), ..] if s - next >= MARGIN => Pick::One(*s, *k, *id),
        [(top, ..), ..] => Pick::Many(
            scored
                .iter()
                .filter(|(s, ..)| top - s < MARGIN)
                .map(|(.., id)| *id)
                .collect(),
        ),
    }
}

enum Pick {
    One(f64, MatchKind, u64),
    Many(Vec<u64>),
    None,
}

pub struct Resolver {
    explicit: HashMap<i64, Explicit>,
    pool: Vec<(MemberInfo, Vec<Key>)>,
    known: HashMap<i64, Resolution>,
    claimed: HashSet<u64>,
}

impl Resolver {
    /// Precomputes matches for `people` (usually the workspace members) so that every member
    /// goes to at most one person. Members used by explicit mappings are never auto matched.
    pub fn new(people: &[User], members: &[MemberInfo], explicit: HashMap<i64, Explicit>) -> Self {
        let used: HashSet<u64> = explicit
            .values()
            .filter_map(|e| match e {
                Explicit::Member(id) => Some(*id),
                Explicit::Never => None,
            })
            .collect();
        let pool: Vec<_> = members
            .iter()
            .filter(|m| !used.contains(&m.id))
            .map(|m| (m.clone(), member_keys(m)))
            .collect();

        let mut known = HashMap::new();
        let mut claims: HashMap<u64, Vec<(f64, MatchKind, i64)>> = HashMap::new();
        for person in people {
            if explicit.contains_key(&person.id) || known.contains_key(&person.id) {
                continue;
            }
            let resolution = match pick(person, pool.iter()) {
                Pick::One(score, kind, member) => {
                    claims
                        .entry(member)
                        .or_default()
                        .push((score, kind, person.id));
                    continue;
                }
                Pick::Many(ids) => Resolution::Ambiguous(ids),
                Pick::None => Resolution::Unmatched,
            };
            known.insert(person.id, resolution);
        }

        let mut claimed = HashSet::new();
        for (member, mut wanted_by) in claims {
            wanted_by.sort_by(|x, y| y.0.total_cmp(&x.0));
            let clear_winner = match wanted_by.as_slice() {
                [_] => true,
                [first, second, ..] => first.0 - second.0 >= MARGIN,
                [] => false,
            };
            for (i, (_, kind, person)) in wanted_by.iter().enumerate() {
                let resolution = if clear_winner && i == 0 {
                    claimed.insert(member);
                    Resolution::Auto {
                        member,
                        kind: *kind,
                    }
                } else {
                    Resolution::Ambiguous(vec![member])
                };
                known.insert(*person, resolution);
            }
        }

        Self {
            explicit,
            pool,
            known,
            claimed,
        }
    }

    /// Discord member for a ClickUp person. People unknown to [`new`](Self::new) are
    /// matched against the members nobody claimed yet.
    pub fn resolve(&self, person: &User) -> Resolution {
        match self.explicit.get(&person.id) {
            Some(Explicit::Member(id)) => return Resolution::Explicit(*id),
            Some(Explicit::Never) => return Resolution::Never,
            None => {}
        }
        if let Some(known) = self.known.get(&person.id) {
            return known.clone();
        }
        let free = self
            .pool
            .iter()
            .filter(|(m, _)| !self.claimed.contains(&m.id));
        match pick(person, free) {
            Pick::One(_, kind, member) => Resolution::Auto { member, kind },
            Pick::Many(ids) => Resolution::Ambiguous(ids),
            Pick::None => Resolution::Unmatched,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(id: i64, username: &str, email: &str) -> User {
        User {
            id,
            username: username.into(),
            email: email.into(),
            initials: String::new(),
        }
    }

    fn member(id: u64, username: &str, global: Option<&str>, nick: Option<&str>) -> MemberInfo {
        MemberInfo {
            id,
            username: username.into(),
            global_name: global.map(Into::into),
            nick: nick.map(Into::into),
        }
    }

    fn auto(people: &[User], members: &[MemberInfo]) -> Resolver {
        Resolver::new(people, members, HashMap::new())
    }

    #[test]
    fn folding_handles_polish_and_friends() {
        assert_eq!(normalize("Igor Zawórski"), "igorzaworski");
        assert_eq!(normalize("Łukasz Żółć"), "lukaszzolc");
        assert_eq!(normalize("Straße Øre"), "strasseore");
        assert_eq!(normalize("igor.zaworski_99"), "igorzaworski99");
        assert_eq!(
            words("Zawórski, Igor").into_iter().collect::<Vec<_>>(),
            ["igor", "zaworski"]
        );
    }

    #[test]
    fn matches_by_name_email_and_word_order() {
        let people = [
            person(1, "Igor Zawórski", "x@example.com"),
            person(2, "", "anna.nowak@example.com"),
            person(3, "Kowalski Jan", "jk@example.com"),
        ];
        let members = [
            member(10, "igor_zaworski", None, None),
            member(20, "an_nowak", Some("Anna Nowak"), None),
            member(30, "jkowal", Some("Jan Kowalski"), None),
        ];
        let r = auto(&people, &members);
        assert_eq!(
            r.resolve(&people[0]),
            Resolution::Auto {
                member: 10,
                kind: MatchKind::Name
            }
        );
        assert_eq!(
            r.resolve(&people[1]),
            Resolution::Auto {
                member: 20,
                kind: MatchKind::Email
            }
        );
        assert_eq!(
            r.resolve(&people[2]),
            Resolution::Auto {
                member: 30,
                kind: MatchKind::Words
            }
        );
    }

    #[test]
    fn nickname_counts_and_partial_names_with_two_words() {
        let people = [person(1, "Anna Nowak", "a@x.io")];
        let members = [member(10, "zzz", None, Some("Anna Nowak (PM)"))];
        assert_eq!(
            auto(&people, &members).resolve(&people[0]),
            Resolution::Auto {
                member: 10,
                kind: MatchKind::Contains
            }
        );
    }

    #[test]
    fn close_typos_match_but_different_surnames_do_not() {
        let people = [person(1, "Igor Zaworski", "i@x.io")];
        let typo = [member(10, "igorzaworsky", None, None)];
        assert!(matches!(
            auto(&people, &typo).resolve(&people[0]),
            Resolution::Auto {
                member: 10,
                kind: MatchKind::Similar
            }
        ));
        let other = [member(10, "igorzawadzki", None, None)];
        assert_eq!(
            auto(&people, &other).resolve(&people[0]),
            Resolution::Unmatched
        );
    }

    #[test]
    fn first_name_alone_is_not_enough() {
        let people = [person(1, "Igor Zaworski", "iz@x.io")];
        let members = [member(10, "igor", Some("Igor"), None)];
        assert_eq!(
            auto(&people, &members).resolve(&people[0]),
            Resolution::Unmatched
        );
        let unrelated = [member(10, "pixel_wizard", Some("Pixel"), None)];
        assert_eq!(
            auto(&people, &unrelated).resolve(&people[0]),
            Resolution::Unmatched
        );
    }

    #[test]
    fn equally_good_members_are_ambiguous() {
        let people = [person(1, "Anna Nowak", "a@x.io")];
        let members = [
            member(10, "anna_nowak", None, None),
            member(20, "anna.nowak", None, None),
        ];
        match auto(&people, &members).resolve(&people[0]) {
            Resolution::Ambiguous(mut ids) => {
                ids.sort();
                assert_eq!(ids, [10, 20]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_member_goes_to_one_person_only() {
        // Both people match the same member, the exact name wins.
        let people = [
            person(1, "Igor Zaworski", "a@x.io"),
            person(2, "Igor Zaworsky", "b@x.io"),
        ];
        let members = [member(10, "igorzaworski", None, None)];
        let r = auto(&people, &members);
        assert_eq!(r.resolve(&people[0]).member(), Some(10));
        assert_eq!(r.resolve(&people[1]), Resolution::Ambiguous(vec![10]));

        // Equal claims: nobody gets the member.
        let twins = [
            person(1, "Anna Nowak", "a@x.io"),
            person(2, "Anna Nowak", "b@x.io"),
        ];
        let r = auto(&twins, &[member(10, "annanowak", None, None)]);
        assert_eq!(r.resolve(&twins[0]), Resolution::Ambiguous(vec![10]));
        assert_eq!(r.resolve(&twins[1]), Resolution::Ambiguous(vec![10]));
    }

    #[test]
    fn explicit_mappings_win_and_take_members_out_of_the_pool() {
        let people = [
            person(1, "Anna Nowak", "a@x.io"),
            person(2, "Igor Zaworski", "i@x.io"),
            person(3, "Jan Kowalski", "j@x.io"),
        ];
        let members = [
            member(10, "annanowak", None, None),
            member(20, "igorzaworski", None, None),
            member(30, "jan.kowalski", None, None),
        ];
        let explicit = HashMap::from([(1, Explicit::Member(30)), (2, Explicit::Never)]);
        let r = Resolver::new(&people, &members, explicit);
        assert_eq!(r.resolve(&people[0]), Resolution::Explicit(30));
        assert_eq!(r.resolve(&people[1]), Resolution::Never);
        // Jan's member is used by Anna's explicit mapping.
        assert_eq!(r.resolve(&people[2]), Resolution::Unmatched);
    }

    #[test]
    fn people_outside_the_workspace_list_use_free_members() {
        let people = [person(1, "Anna Nowak", "a@x.io")];
        let members = [
            member(10, "annanowak", None, None),
            member(20, "piotr.wisniewski", Some("Piotr Wiśniewski"), None),
        ];
        let r = auto(&people, &members);
        let guest = person(99, "Piotr Wisniewski", "p@x.io");
        assert_eq!(r.resolve(&guest).member(), Some(20));
        // Anna's member is taken.
        let anna_again = person(98, "Anna Nowak", "other@x.io");
        assert_eq!(r.resolve(&anna_again), Resolution::Unmatched);
    }

    #[test]
    fn short_names_are_ignored() {
        let people = [person(1, "Al", "al@x.io")];
        let members = [member(10, "al", None, None)];
        assert_eq!(
            auto(&people, &members).resolve(&people[0]),
            Resolution::Unmatched
        );
    }
}
