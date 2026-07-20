//! RFC 3339 UTC timestamps, used wherever PLAN.md requires one
//! (e.g. §5.4 `matched_at_utc`, §7.2 `Workset::created_at`, §7.4 journal `created_at`).

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("RFC 3339 formatting of the current time cannot fail")
}
