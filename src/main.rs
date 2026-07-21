//! Thin binary entry point. All argument parsing and command logic lives in
//! the library crate (`breadcrumbs::app`) so it can also be exercised
//! in-process by the integration tests under `tests/`.

fn main() {
    std::process::exit(breadcrumbs::app::run());
}
