//! JNI entry for hosting the Shenko node inside the Android APK
//! (SPEC §24.5, docs/specs/mobile-runtime.md).
//!
//! The Kotlin `ShenkoNodeService` loads this cdylib and calls
//! `nativeStart(filesDir, port)` on a dedicated thread. We build a Tokio
//! runtime here and drive the *same* library runtime the CLI binary uses
//! (`shenko::run(Command::Start, filesDir)`) — no second runtime assembly,
//! no renamed executable, no fork. The app files dir is the node data dir,
//! so the store, CAS, and bridge socket (`<filesDir>/device_bridge.sock`)
//! live in app-private storage owned by this uid.
//!
//! This target is intentionally empty on non-Android hosts: the workspace
//! must keep building on desktop CI while the real code path only links
//! `jni` and `shenko` for Android targets.

#[cfg(target_os = "android")]
mod android {
    use jni::objects::{JClass, JString};
    use jni::sys::{jint, jstring};
    use jni::JNIEnv;

    /// Start the node. Called once by `ShenkoNodeService` on its worker
    /// thread; returns a short status string for the service log and blocks
    /// until the runtime exits (service stop tears the thread down).
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_org_shenko_node_ShenkoNodeService_nativeStart(
        mut env: JNIEnv,
        _class: JClass,
        files_dir: JString,
        port: jint,
    ) -> jstring {
        let files_dir: String = match env.get_string(&files_dir) {
            Ok(value) => value.into(),
            Err(error) => {
                return env
                    .new_string(format!("error: bad filesDir: {error}"))
                    .map(|value| value.into_raw())
                    .unwrap_or(std::ptr::null_mut());
            }
        };
        let data_dir = std::path::PathBuf::from(files_dir);
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("shenko-node")
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                return env
                    .new_string(format!("error: runtime build failed: {error}"))
                    .map(|value| value.into_raw())
                    .unwrap_or(std::ptr::null_mut());
            }
        };
        let result = runtime.block_on(shenko::run(
            shenko::Command::Start {
                port: u16::try_from(port).unwrap_or(7860),
                name: None,
                napcat: None,
                napcat_account: None,
            },
            data_dir,
        ));
        let status = match result {
            Ok(()) => "stopped".to_string(),
            Err(error) => format!("error: {error:#}"),
        };
        env.new_string(status)
            .map(|value| value.into_raw())
            .unwrap_or(std::ptr::null_mut())
    }
}
