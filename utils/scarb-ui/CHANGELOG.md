# Changelog

All notable changes to this project will be documented in this file.

## Unreleased

## 0.1.6 (2024-07-10)
- Make `Verbosity` a `clap::ValueEnum`.
- Add `Verbosity::is_default` method.
- Add the `NewLine` message that prints a new line.
- Update the Rust edition to `2024`.
- Add `Verbosity::NoWarnings` variant, that hides warnings from the output.
- Keep progress bar state in `Ui`, used it for printing the output when the progress is not finished.

## 0.1.5 (2024-04-23)
- Fixed log verbosity calculation.
- Added support for diagnostics error codes.
- Added `ToEnvVars` trait.

## 0.1.4 (2024-04-09)
- Added `FeaturesSpec` and `VerbositySpec` parsers.

## 0.1.3 (2024-01-22)
- Added `Ui::force_colors_enabled_stderr` and `Ui::has_colors_enabled_stderr`.

## 0.1.2 (2023-11-14)
- Added `PackagesFilterLong` parser.

## 0.1.1 (2023-10-31)
- Added `Clone` implementation for `Ui`.

## 0.1.0 (2023-10-05)
- Initial release.
