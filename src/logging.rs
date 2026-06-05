/// Log a `[twelf]`-prefixed diagnostic to stderr in debug builds only.
///
/// The `if cfg!(debug_assertions)` keeps the arguments type-checked in every
/// configuration, while release builds optimize the branch out and stay silent.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        if cfg!(debug_assertions) {
            eprintln!("[twelf] {}", format_args!($($arg)*));
        }
    };
}
