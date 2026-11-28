//! What a user copy reports for each kind of fault it could not resolve (OD161, T285), checked against the table in SYSCALLS.md ("Copying
//! from and to user memory"): the documented status of every `FaultError` variant is the one `kcore::usercopy::fault_status` gives, every
//! variant has a row, and no row names a variant that does not exist. A new variant fails to compile in `every_variant` until it is given a
//! row here and in the reference.
//!
//! Mutation check (by hand, T285): map `VmoGone` to NO_MEMORY in `fault_status` and the test fails on that row.
use abi::status::Status;
use kcore::aspace::{FaultError, PagerNeed};
use kcore::pagetable::MapError;
use kcore::usercopy::fault_status;

/// One value of each variant, by its name in the reference (the `match` makes the list exhaustive).
fn every_variant() -> Vec<(&'static str, FaultError)> {
    let all = [
        FaultError::NoRegion,
        FaultError::Denied,
        FaultError::VmoGone,
        FaultError::NoMemory,
        FaultError::MapFailed(MapError::AlreadyMapped),
        FaultError::NeedPager(PagerNeed {
            pager: 1,
            vmo: 2,
            page: 3,
        }),
    ];
    all.into_iter()
        .map(|e| {
            let name = match e {
                FaultError::NoRegion => "NoRegion",
                FaultError::Denied => "Denied",
                FaultError::VmoGone => "VmoGone",
                FaultError::NoMemory => "NoMemory",
                FaultError::MapFailed(_) => "MapFailed",
                FaultError::NeedPager(_) => "NeedPager",
            };
            (name, e)
        })
        .collect()
}

fn status_named(s: &str) -> Status {
    match s {
        "INVALID_ARGS" => Status::InvalidArgs,
        "NO_MEMORY" => Status::NoMemory,
        "SHOULD_WAIT" => Status::ShouldWait,
        other => panic!("SYSCALLS.md names a status this test does not know: {other}"),
    }
}

/// The rows of the table: (variant, documented status).
fn documented() -> Vec<(String, Status)> {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../docs/reference/SYSCALLS.md"
    ))
    .expect("SYSCALLS.md");
    let start = text
        .find("| What the walk found | Status |")
        .expect("the fault table in SYSCALLS.md, \"Copying from and to user memory\"");
    text[start..]
        .lines()
        .skip(2)
        .take_while(|l| l.starts_with('|'))
        .map(|l| {
            let cells: Vec<&str> = l.split('|').map(str::trim).collect();
            let name = cells[1]
                .split('`')
                .nth(1)
                .expect("a row names its variant in backticks");
            (name.to_string(), status_named(cells[2]))
        })
        .collect()
}

#[test]
fn every_fault_reports_the_status_syscalls_md_documents() {
    let rows = documented();
    let variants = every_variant();
    for (name, e) in &variants {
        let row = rows
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("SYSCALLS.md has no row for `{name}`"));
        assert_eq!(fault_status(e), row.1, "`{name}`");
    }
    for (n, _) in &rows {
        assert!(
            variants.iter().any(|(v, _)| v == n),
            "SYSCALLS.md has a row for `{n}`, which is not a FaultError"
        );
    }
    // (OD161) a bad address is never reported as exhaustion, and exhaustion never as a bad address
    assert_eq!(fault_status(&FaultError::VmoGone), Status::InvalidArgs);
    assert_eq!(fault_status(&FaultError::NoMemory), Status::NoMemory);
}
