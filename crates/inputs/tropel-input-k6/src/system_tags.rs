//! # k6 `options.systemTags`
//!
//! TR-212. k6 lets a script choose which *system* tags are stamped onto every
//! sample:
//!
//! ```js
//! export const options = { systemTags: ['url', 'status'] };
//! ```
//!
//! This is not cosmetic. System tags are the series dimensions — they decide
//! threshold selectors (`http_req_duration{status:200}`), the cardinality of
//! every downstream dashboard, and the size of the egress stream. A script
//! that trims the set is trimming its own metric bill, and a script that adds
//! `vu`/`iter` is asking for per-VU breakdown.
//!
//! Tropel previously ignored the option entirely and stamped a fixed set —
//! `CONTEXT.md` invariant 3 ("never declare a capability that isn't
//! forwarded"). The k6 semantics implemented here:
//!
//! * **Absent** → [`SystemTagSet::k6_default`], the 14 tags k6 enables by
//!   default.
//! * **Present** → the listed tags *replace* the default set. It is not
//!   additive; `systemTags: ['url']` yields exactly one system tag.
//! * **Present and empty** (`systemTags: []`) → no system tags at all. This is
//!   why the option is modelled as `Option<Vec<String>>` and not `Vec<String>`:
//!   "absent" and "explicitly empty" are different instructions.
//! * **Unknown name** → warn and ignore that name, keeping the rest. k6 hard-
//!   errors here; tropel warns because dropping the whole list would silently
//!   restore the default set, which is the failure mode this task exists to
//!   remove.
//!
//! User tags are *never* filtered by this set. `params.tags`, `exec.vu.tags`
//! and scenario `tags` are the user's own dimensions; `systemTags` governs only
//! the tags tropel adds on the user's behalf.

/// One k6 system tag. Discriminants are bit positions in [`SystemTagSet`].
///
/// The names are k6's exact wire names (`metrics/system_tag.go`) — they appear
/// in threshold selectors and in every exported sample, so a rename here is a
/// breaking metric-contract change, not a refactor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum SystemTag {
    Proto = 1 << 0,
    Subproto = 1 << 1,
    Status = 1 << 2,
    Method = 1 << 3,
    Url = 1 << 4,
    Name = 1 << 5,
    Group = 1 << 6,
    Check = 1 << 7,
    Error = 1 << 8,
    ErrorCode = 1 << 9,
    TlsVersion = 1 << 10,
    Scenario = 1 << 11,
    Service = 1 << 12,
    ExpectedResponse = 1 << 13,
    // ── Available but OFF by default in k6 ──
    Vu = 1 << 14,
    Iter = 1 << 15,
    Ip = 1 << 16,
    OcspStatus = 1 << 17,
}

impl SystemTag {
    /// k6's wire name for this tag.
    pub const fn name(self) -> &'static str {
        match self {
            SystemTag::Proto => "proto",
            SystemTag::Subproto => "subproto",
            SystemTag::Status => "status",
            SystemTag::Method => "method",
            SystemTag::Url => "url",
            SystemTag::Name => "name",
            SystemTag::Group => "group",
            SystemTag::Check => "check",
            SystemTag::Error => "error",
            SystemTag::ErrorCode => "error_code",
            SystemTag::TlsVersion => "tls_version",
            SystemTag::Scenario => "scenario",
            SystemTag::Service => "service",
            SystemTag::ExpectedResponse => "expected_response",
            SystemTag::Vu => "vu",
            SystemTag::Iter => "iter",
            SystemTag::Ip => "ip",
            SystemTag::OcspStatus => "ocsp_status",
        }
    }

    /// Parse a k6 system-tag name. `None` for anything k6 doesn't define.
    pub fn from_name(s: &str) -> Option<SystemTag> {
        Some(match s {
            "proto" => SystemTag::Proto,
            "subproto" => SystemTag::Subproto,
            "status" => SystemTag::Status,
            "method" => SystemTag::Method,
            "url" => SystemTag::Url,
            "name" => SystemTag::Name,
            "group" => SystemTag::Group,
            "check" => SystemTag::Check,
            "error" => SystemTag::Error,
            "error_code" => SystemTag::ErrorCode,
            "tls_version" => SystemTag::TlsVersion,
            "scenario" => SystemTag::Scenario,
            "service" => SystemTag::Service,
            "expected_response" => SystemTag::ExpectedResponse,
            "vu" => SystemTag::Vu,
            "iter" => SystemTag::Iter,
            "ip" => SystemTag::Ip,
            "ocsp_status" => SystemTag::OcspStatus,
            _ => return None,
        })
    }

    /// Every tag k6 defines, in wire order. Used for the unknown-name warning
    /// so the message can list what *is* accepted.
    pub const ALL: [SystemTag; 18] = [
        SystemTag::Proto,
        SystemTag::Subproto,
        SystemTag::Status,
        SystemTag::Method,
        SystemTag::Url,
        SystemTag::Name,
        SystemTag::Group,
        SystemTag::Check,
        SystemTag::Error,
        SystemTag::ErrorCode,
        SystemTag::TlsVersion,
        SystemTag::Scenario,
        SystemTag::Service,
        SystemTag::ExpectedResponse,
        SystemTag::Vu,
        SystemTag::Iter,
        SystemTag::Ip,
        SystemTag::OcspStatus,
    ];
}

/// A set of enabled [`SystemTag`]s, as a bitmask.
///
/// A bitmask rather than a `HashSet<String>` because [`SystemTagSet::has`] is
/// called several times per HTTP sample on the hot path — it compiles to a
/// single `and`/`test`, with no hashing and no allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemTagSet(u32);

impl SystemTagSet {
    /// k6's `DefaultSystemTagSet` — the 14 tags enabled when a script says
    /// nothing. `vu`, `iter`, `ip` and `ocsp_status` are deliberately absent:
    /// k6 defines them but leaves them off, because `vu`/`iter` multiply
    /// series count by the VU count and `ip` by the backend-pool size.
    pub const fn k6_default() -> Self {
        SystemTagSet(
            SystemTag::Proto as u32
                | SystemTag::Subproto as u32
                | SystemTag::Status as u32
                | SystemTag::Method as u32
                | SystemTag::Url as u32
                | SystemTag::Name as u32
                | SystemTag::Group as u32
                | SystemTag::Check as u32
                | SystemTag::Error as u32
                | SystemTag::ErrorCode as u32
                | SystemTag::TlsVersion as u32
                | SystemTag::Scenario as u32
                | SystemTag::Service as u32
                | SystemTag::ExpectedResponse as u32,
        )
    }

    /// The empty set — `systemTags: []`.
    pub const fn empty() -> Self {
        SystemTagSet(0)
    }

    /// Is this tag enabled?
    #[inline]
    pub const fn has(self, tag: SystemTag) -> bool {
        self.0 & (tag as u32) != 0
    }

    /// Add a tag (used by the parser and by tests).
    pub const fn with(self, tag: SystemTag) -> Self {
        SystemTagSet(self.0 | tag as u32)
    }

    /// Parse a k6 `systemTags` list. Returns the set plus every unrecognized
    /// name, so the caller can warn once with the offending names.
    ///
    /// Unknown names are dropped rather than failing the parse: failing would
    /// fall back to the default set, which is exactly the silent
    /// "option-ignored" behaviour TR-212 removes. A typo must shrink the set
    /// visibly, not restore all 14.
    pub fn parse(names: &[String]) -> (Self, Vec<String>) {
        let mut set = SystemTagSet::empty();
        let mut unknown = Vec::new();
        for raw in names {
            let name = raw.trim();
            match SystemTag::from_name(name) {
                Some(tag) => set = set.with(tag),
                None => unknown.push(raw.clone()),
            }
        }
        (set, unknown)
    }

    /// Resolve the effective set from the script's declared option.
    ///
    /// `None` (no `systemTags` key) → the k6 default set. `Some(list)` → the
    /// listed tags *replace* the defaults, including `Some([])` → nothing.
    /// Warns on unknown names.
    pub fn resolve(declared: Option<&Vec<String>>) -> Self {
        let Some(names) = declared else {
            return SystemTagSet::k6_default();
        };
        let (set, unknown) = SystemTagSet::parse(names);
        if !unknown.is_empty() {
            let known: Vec<&str> = SystemTag::ALL.iter().map(|t| t.name()).collect();
            tracing::warn!(
                "k6 option systemTags contains {} unrecognized name(s): {} — ignored. \
                 Valid names: {}",
                unknown.len(),
                unknown.join(", "),
                known.join(", ")
            );
        }
        set
    }

    /// The enabled tags, for diagnostics.
    pub fn names(self) -> Vec<&'static str> {
        SystemTag::ALL
            .iter()
            .filter(|t| self.has(**t))
            .map(|t| t.name())
            .collect()
    }
}

impl Default for SystemTagSet {
    fn default() -> Self {
        SystemTagSet::k6_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default set is k6's 14 — and specifically NOT `vu`/`iter`/`ip`/
    /// `ocsp_status`, which k6 defines but leaves off to bound cardinality.
    #[test]
    fn k6_default_set_is_the_documented_fourteen() {
        let d = SystemTagSet::k6_default();
        assert_eq!(
            d.names(),
            vec![
                "proto",
                "subproto",
                "status",
                "method",
                "url",
                "name",
                "group",
                "check",
                "error",
                "error_code",
                "tls_version",
                "scenario",
                "service",
                "expected_response",
            ],
            "the default set must be k6's DefaultSystemTagSet, exactly"
        );
        for off in [
            SystemTag::Vu,
            SystemTag::Iter,
            SystemTag::Ip,
            SystemTag::OcspStatus,
        ] {
            assert!(
                !d.has(off),
                "{} must be OFF by default (cardinality)",
                off.name()
            );
        }
    }

    /// `systemTags` REPLACES the default set — it is not additive. A script
    /// asking for two tags gets two, not sixteen.
    #[test]
    fn declared_list_replaces_the_default_set() {
        let set = SystemTagSet::resolve(Some(&vec!["url".into(), "status".into()]));
        assert_eq!(set.names(), vec!["status", "url"]);
        assert!(!set.has(SystemTag::Method), "method was not requested");
        assert!(!set.has(SystemTag::Scenario), "scenario was not requested");
    }

    /// `systemTags: []` means none — distinct from the option being absent.
    #[test]
    fn explicit_empty_list_disables_every_system_tag() {
        let empty = SystemTagSet::resolve(Some(&vec![]));
        assert!(empty.names().is_empty(), "explicit [] must yield no tags");
        let absent = SystemTagSet::resolve(None);
        assert_eq!(
            absent.names().len(),
            14,
            "absent must fall back to the default 14, not to empty"
        );
    }

    /// A typo shrinks the set and is reported — it must NOT silently restore
    /// all 14, which is the bug this option's absence caused for years.
    #[test]
    fn unknown_name_is_reported_and_does_not_restore_defaults() {
        let (set, unknown) =
            SystemTagSet::parse(&["url".into(), "stat_us".into(), "protocol".into()]);
        assert_eq!(
            unknown,
            vec!["stat_us".to_string(), "protocol".to_string()],
            "both typos must be reported"
        );
        assert_eq!(set.names(), vec!["url"], "only the valid name survives");
        assert!(
            !set.has(SystemTag::Status),
            "a typo must not enable the tag it resembles"
        );
    }

    /// Every k6 name round-trips. Guards against a discriminant collision —
    /// two tags sharing a bit would make one silently enable the other.
    #[test]
    fn every_tag_name_round_trips_and_owns_a_unique_bit() {
        let mut seen = 0u32;
        for tag in SystemTag::ALL {
            assert_eq!(
                SystemTag::from_name(tag.name()),
                Some(tag),
                "{} must round-trip",
                tag.name()
            );
            let bit = tag as u32;
            assert_eq!(seen & bit, 0, "{} reuses another tag's bit", tag.name());
            seen |= bit;
        }
        assert_eq!(seen.count_ones(), 18);
    }
}
