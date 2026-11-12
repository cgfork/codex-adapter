# proc-macro-error2 local patch

This directory contains the source and license files from the crates.io release
of `proc-macro-error2` 2.0.1. The only source change is in `src/lib.rs`: the
`proc_macro` extern crate is public and hidden from generated documentation.
This is the fix proposed in [upstream pull request #14](https://github.com/GnomedDev/proc-macro-error-2/pull/14)
at commit `53ff94b61c678593158db1a3d50269c6f0de0db7` for Rust's E0365
future-incompatibility warning. The original repository is archived, and the
fix has not been released to crates.io.

The package manifest omits test targets and development dependencies because
the release's test files are not included here. The runtime dependencies and
features are unchanged.
# Packaging note

`LICENSE-APACHE` uses LF line endings; its license text is unchanged.
