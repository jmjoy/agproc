# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0](https://github.com/jmjoy/agproc/compare/v0.3.0...v0.4.0) - 2026-10-23

### Added

- support macOS alongside Linux for process management

### Fixed

- handle macOS process group exit states ([#6](https://github.com/jmjoy/agproc/pull/6))
- handle process group enumeration failures correctly on macOS
- make the macOS test job pass
- classify stopped builds as cancelled

### Other

- merge test job into check job and rename to 'check'
- pin CI to Rust 1.95.0 and use latest runner images
- 添加自动化 CI 与发布工作流
- Refactor logging and process handling for combined output
