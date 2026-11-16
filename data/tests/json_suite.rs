//! The independent oracle for the JSON parser: JSONTestSuite's `test_parsing` (vendored under `third_party/json-test-suite`,
//! SHELL-SPEC section 6). `y_*` files must be accepted and `n_*` files rejected; `i_*` files are implementation-defined, so
//! each is pinned here with the reason. Two `y_*` files are rejected on purpose (a duplicate key is an error, SHELL-SPEC
//! section 4 rule 6), and so is any number past 38 digits (`NumberTooPrecise`).

use data::{from_json, to_json, ErrorKind, Limits};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../third_party/json-test-suite/test_parsing")
}

fn files(prefix: &str) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    for e in fs::read_dir(dir()).expect("the vendored suite") {
        let e = e.unwrap();
        let name = e.file_name().into_string().unwrap();
        if name.starts_with(prefix) && name.ends_with(".json") {
            out.insert(name, fs::read(e.path()).unwrap());
        }
    }
    out
}

/// `y_*` files this parser rejects, with the kind of error and why.
const Y_REJECTED: &[(&str, ErrorKind, &str)] = &[
    // numbers are exact decimals of at most 38 digits: these would need 65 to 80
    (
        "y_number.json",
        ErrorKind::NumberTooPrecise,
        "123e65 is 66 digits",
    ),
    (
        "y_number_double_close_to_zero.json",
        ErrorKind::NumberTooPrecise,
        "80 fractional digits",
    ),
    (
        "y_number_real_exponent.json",
        ErrorKind::NumberTooPrecise,
        "123e45 is 48 digits",
    ),
    (
        "y_number_real_fraction_exponent.json",
        ErrorKind::NumberTooPrecise,
        "123.456e78 is 79 digits",
    ),
    // SHELL-SPEC section 4 rule 6: a duplicate key is an error
    (
        "y_object_duplicated_key.json",
        ErrorKind::DuplicateKey,
        "rule 6",
    ),
    (
        "y_object_duplicated_key_and_value.json",
        ErrorKind::DuplicateKey,
        "rule 6",
    ),
];

/// Every `i_*` file: whether it is accepted, and why.
const I_PINNED: &[(&str, bool, &str)] = &[
    (
        "i_number_double_huge_neg_exp.json",
        false,
        "past the 38-digit cap on digits or scale (an exponent that large, or an overflow)",
    ),
    (
        "i_number_huge_exp.json",
        false,
        "past the 38-digit cap on digits or scale (an exponent that large, or an overflow)",
    ),
    (
        "i_number_neg_int_huge_exp.json",
        false,
        "past the 38-digit cap on digits or scale (an exponent that large, or an overflow)",
    ),
    (
        "i_number_pos_double_huge_exp.json",
        false,
        "past the 38-digit cap on digits or scale (an exponent that large, or an overflow)",
    ),
    (
        "i_number_real_neg_overflow.json",
        false,
        "past the 38-digit cap on digits or scale (an exponent that large, or an overflow)",
    ),
    (
        "i_number_real_pos_overflow.json",
        false,
        "past the 38-digit cap on digits or scale (an exponent that large, or an overflow)",
    ),
    (
        "i_number_real_underflow.json",
        false,
        "past the 38-digit cap on digits or scale (an exponent that large, or an overflow)",
    ),
    (
        "i_number_too_big_neg_int.json",
        true,
        "a 30-digit integer is an exact decimal (38 digits at most)",
    ),
    (
        "i_number_too_big_pos_int.json",
        true,
        "a 30-digit integer is an exact decimal (38 digits at most)",
    ),
    (
        "i_number_very_big_negative_int.json",
        false,
        "past the 38-digit cap on digits or scale (an exponent that large, or an overflow)",
    ),
    (
        "i_object_key_lone_2nd_surrogate.json",
        false,
        "a lone or misordered surrogate cannot be a UTF-8 string",
    ),
    (
        "i_string_1st_surrogate_but_2nd_missing.json",
        false,
        "a lone or misordered surrogate cannot be a UTF-8 string",
    ),
    (
        "i_string_1st_valid_surrogate_2nd_invalid.json",
        false,
        "a lone or misordered surrogate cannot be a UTF-8 string",
    ),
    (
        "i_string_UTF-16LE_with_BOM.json",
        false,
        "no BOM; the input is UTF-8 text without one",
    ),
    (
        "i_string_UTF-8_invalid_sequence.json",
        false,
        "not valid UTF-8 (strings are UTF-8, SHELL-SPEC section 4 rule 4)",
    ),
    (
        "i_string_UTF8_surrogate_U+D800.json",
        false,
        "a lone or misordered surrogate cannot be a UTF-8 string",
    ),
    (
        "i_string_incomplete_surrogate_and_escape_valid.json",
        false,
        "a lone or misordered surrogate cannot be a UTF-8 string",
    ),
    (
        "i_string_incomplete_surrogate_pair.json",
        false,
        "a lone or misordered surrogate cannot be a UTF-8 string",
    ),
    (
        "i_string_incomplete_surrogates_escape_valid.json",
        false,
        "a lone or misordered surrogate cannot be a UTF-8 string",
    ),
    (
        "i_string_invalid_lonely_surrogate.json",
        false,
        "a lone or misordered surrogate cannot be a UTF-8 string",
    ),
    (
        "i_string_invalid_surrogate.json",
        false,
        "a lone or misordered surrogate cannot be a UTF-8 string",
    ),
    (
        "i_string_invalid_utf-8.json",
        false,
        "not valid UTF-8 (strings are UTF-8, SHELL-SPEC section 4 rule 4)",
    ),
    (
        "i_string_inverted_surrogates_U+1D11E.json",
        false,
        "a lone or misordered surrogate cannot be a UTF-8 string",
    ),
    (
        "i_string_iso_latin_1.json",
        false,
        "not valid UTF-8 (strings are UTF-8, SHELL-SPEC section 4 rule 4)",
    ),
    (
        "i_string_lone_second_surrogate.json",
        false,
        "a lone or misordered surrogate cannot be a UTF-8 string",
    ),
    (
        "i_string_lone_utf8_continuation_byte.json",
        false,
        "not valid UTF-8 (strings are UTF-8, SHELL-SPEC section 4 rule 4)",
    ),
    (
        "i_string_not_in_unicode_range.json",
        false,
        "not valid UTF-8 (strings are UTF-8, SHELL-SPEC section 4 rule 4)",
    ),
    (
        "i_string_overlong_sequence_2_bytes.json",
        false,
        "not valid UTF-8 (strings are UTF-8, SHELL-SPEC section 4 rule 4)",
    ),
    (
        "i_string_overlong_sequence_6_bytes.json",
        false,
        "not valid UTF-8 (strings are UTF-8, SHELL-SPEC section 4 rule 4)",
    ),
    (
        "i_string_overlong_sequence_6_bytes_null.json",
        false,
        "not valid UTF-8 (strings are UTF-8, SHELL-SPEC section 4 rule 4)",
    ),
    (
        "i_string_truncated-utf-8.json",
        false,
        "not valid UTF-8 (strings are UTF-8, SHELL-SPEC section 4 rule 4)",
    ),
    (
        "i_string_utf16BE_no_BOM.json",
        false,
        "no BOM; the input is UTF-8 text without one",
    ),
    (
        "i_string_utf16LE_no_BOM.json",
        false,
        "no BOM; the input is UTF-8 text without one",
    ),
    (
        "i_structure_500_nested_arrays.json",
        false,
        "nesting is capped at 128 levels",
    ),
    (
        "i_structure_UTF-8_BOM_empty_object.json",
        false,
        "no BOM; the input is UTF-8 text without one",
    ),
];

#[test]
fn the_suite_is_all_there() {
    assert_eq!(files("y_").len(), 95);
    assert_eq!(files("n_").len(), 188);
    assert_eq!(files("i_").len(), 35);
}

#[test]
fn every_y_file_is_accepted_except_the_listed() {
    let lim = Limits::DEFAULT;
    let mut report = Vec::new();
    for (name, data) in files("y_") {
        let r = from_json(&data, &lim);
        let listed = Y_REJECTED.iter().find(|(n, _, _)| *n == name);
        match (&r, listed) {
            (Ok(_), None) => {}
            (Err(e), Some((_, k, _))) if e.kind == *k => {}
            (r, l) => report.push(format!("{name}: {r:?} (listed: {l:?})")),
        }
    }
    assert!(report.is_empty(), "{}", report.join("\n"));
}

#[test]
fn every_n_file_is_rejected() {
    let lim = Limits::DEFAULT;
    let bad: Vec<_> = files("n_")
        .into_iter()
        .filter(|(_, d)| from_json(d, &lim).is_ok())
        .map(|(n, _)| n)
        .collect();
    assert!(bad.is_empty(), "accepted: {bad:?}");
}

#[test]
fn every_i_file_is_pinned_and_behaves_as_pinned() {
    let lim = Limits::DEFAULT;
    let all = files("i_");
    let mut report = Vec::new();
    for (name, data) in &all {
        match I_PINNED.iter().find(|(n, _, _)| n == name) {
            None => report.push(format!(
                "{name}: not pinned (accepted: {})",
                from_json(data, &lim).is_ok()
            )),
            Some((_, accept, _)) => {
                if from_json(data, &lim).is_ok() != *accept {
                    report.push(format!("{name}: expected accept={accept}"));
                }
            }
        }
    }
    for (n, _, _) in I_PINNED {
        if !all.contains_key(*n) {
            report.push(format!("{n}: pinned but not in the suite"));
        }
    }
    assert!(report.is_empty(), "{}", report.join("\n"));
}

#[test]
fn accepted_documents_survive_a_round_trip() {
    // parse, write, parse again: the same value (the oracle's own accepted files, not just our examples)
    let lim = Limits::DEFAULT;
    for prefix in ["y_", "i_"] {
        for (name, data) in files(prefix) {
            if let Ok(v) = from_json(&data, &lim) {
                let text = to_json(&v, &lim).unwrap_or_else(|e| panic!("{name}: emit: {e:?}"));
                let again = from_json(text.as_bytes(), &lim)
                    .unwrap_or_else(|e| panic!("{name}: {e}: {text}"));
                assert_eq!(v, again, "{name}");
            }
        }
    }
}
