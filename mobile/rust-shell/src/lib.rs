//! JNI entry for hosting the Shenko node inside the Android APK
//! (SPEC §24.5, docs/specs/mobile-runtime.md).
//!
//! The Kotlin `ShenkoNodeService` loads this cdylib and calls
//! `nativeStart(filesDir, port)` on a dedicated thread. The shared owned host
//! builds the Tokio runtime and drives the same assembly as the CLI
//! (`shenko::run_owned(Command::Start, filesDir)`) — no second runtime assembly,
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
    /// until the owned runtime exits. Service destruction alone is not
    /// evidence that this native call or its workers have ended.
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
        let port = match u16::try_from(port) {
            Ok(port) => port,
            Err(error) => {
                return env
                    .new_string(format!("error: invalid port: {error}"))
                    .map(|value| value.into_raw())
                    .unwrap_or(std::ptr::null_mut());
            }
        };
        let result = shenko::run_owned(
            shenko::Command::Start {
                port,
                name: None,
                napcat: None,
                napcat_account: None,
            },
            data_dir,
        );
        let status = match result {
            Ok(()) => "stopped".to_string(),
            Err(error) => format!("error: {error:#}"),
        };
        env.new_string(status)
            .map(|value| value.into_raw())
            .unwrap_or(std::ptr::null_mut())
    }
}
