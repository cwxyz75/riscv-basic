# JSONTestSuite `test_parsing` (vendored, unmodified)

- Source: <https://github.com/nst/JSONTestSuite>, commit `1ef36fa01286573e846ac449e8683f8833c5b26a`, taken 2026-11-17.
- Licence: MIT (`LICENSE`, Nicolas Seriot).
- Contents here: the 318 files of `test_parsing/` exactly as upstream. `y_*` must be accepted, `n_*` must be rejected,
  `i_*` are implementation-defined.
- Used by `data/tests/json_suite.rs` (the independent oracle for the `data` crate, SHELL-SPEC section 6). Every `i_*` file and
  every `y_*` file that `data` rejects is listed there with the reason (strings are UTF-8, duplicate keys are an error,
  numbers are exact decimals with a digit cap, nesting is capped).
