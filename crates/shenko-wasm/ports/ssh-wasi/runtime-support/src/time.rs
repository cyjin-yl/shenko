#[cfg(any(not(target_arch = "wasm32"), target_os = "wasi"))]
pub use std::time::Instant;

#[cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]
pub use wasm::Instant;

#[cfg(all(target_arch = "wasm32", not(target_os = "wasi")))]
mod wasm {
    #[derive(Debug, Clone, Copy)]
    pub struct Instant {
        inner: chrono::DateTime<chrono::Utc>,
    }

    impl Instant {
        pub fn now() -> Self {
            Instant {
                inner: chrono::Utc::now(),
            }
        }

        pub fn duration_since(&self, earlier: Instant) -> std::time::Duration {
            (self.inner - earlier.inner)
                .to_std()
                .expect("Duration is negative")
        }
    }
}
