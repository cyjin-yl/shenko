// Public-only owned fixture. Never generate/read a private key or connect.
#[cfg(unix)]
mod ssh_public_identity {
    use super::*;
    use std::os::unix::ffi::OsStrExt as _;

    const PUBLIC: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINdamAGCsQq31Uv+08lkBzoO4XLz2qYjJa8CGmj3B1Ea";
    const FINGERPRINT: &str = "SHA256:bbXpuKG6zhzdmnxq256TlqzFBzRl2f6OOg722cYNbU8";
    const COMMENT: &str = "private-note-not-for-output";

    struct Fixture { root: PathBuf, config: PathBuf, data: PathBuf }
    impl Fixture {
        fn new(public: &str, duplicate: bool) -> Self {
            let root = std::env::temp_dir().join(format!("shenko-public-only-{}", EntityId::new()));
            std::fs::create_dir(&root).unwrap();
            let private = root.join("not-opened-private-key-fifo");
            let trust = root.join("not-opened-known-hosts-fifo");
            // No FIFO writer exists. Invoking the actual installer would fail
            // its regular-file guard; the public-only CLI must not invoke it.
            for path in [&private, &trust] {
                let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            }
            let target = serde_json::json!({"node_id":EntityId::new(), "target_id":"owned-public-fixture",
                "revision":"owned-public-only-v1", "host":"not-contacted.invalid", "port":22,
                "account":"not-authenticated", "private_key_file":private,
                "public_key_openssh":public, "known_hosts_file":trust,
                "receipt_root":"/not-opened-owned-receipts"});
            let targets = if duplicate { vec![target.clone(), target] } else { vec![target] };
            let config = root.join("explicit-installation.json");
            std::fs::write(&config, serde_json::to_vec(&serde_json::json!({
                "schema":"shenko.ssh.file-installation.v1", "targets":targets})).unwrap()).unwrap();
            let data = root.join("must-not-create-default-store");
            Self { root, config, data }
        }
        fn cli(&self) -> Cli {
            Cli::try_parse_from(["shenko", "--data-dir", self.data.to_str().unwrap(),
                "ssh-public-identity", "--installation", self.config.to_str().unwrap(),
                "--target", "owned-public-fixture"]).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.root); }
    }

    #[test]
    fn cli_displays_only_configured_public_identity_without_opening_signer_or_trust() {
        let f = Fixture::new(&format!("{PUBLIC} {COMMENT}"), false);
        let before = std::fs::read(&f.config).unwrap();
        let cli = f.cli();
        let output = configured_ssh_public_identity_output(&cli.command).unwrap().unwrap();
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value, serde_json::json!({"schema":"ssh.configured-public-identity.v1",
            "status":"configured_unverified", "target_id":"owned-public-fixture",
            "algorithm":"ssh-ed25519", "public_key_openssh":PUBLIC, "fingerprint_sha256":FINGERPRINT}));
        assert!(!output.contains(COMMENT));
        assert!(!output.contains(f.root.to_str().unwrap()));
        assert!(!f.data.exists());
        assert_eq!(std::fs::read(&f.config).unwrap(), before);
    }

    #[test]
    fn cli_rejects_duplicate_selected_target_without_disclosing_config() {
        let f = Fixture::new(PUBLIC, true);
        let error = configured_ssh_public_identity_output(&f.cli().command).unwrap_err();
        assert_eq!(error.to_string(), "ssh_public_identity_target_ambiguous");
        assert!(!f.data.exists());
    }

    #[test]
    fn cli_rejects_malformed_public_key_without_echoing_its_contents() {
        let f = Fixture::new("owned-invalid-public-input-not-for-error-output", false);
        let error = configured_ssh_public_identity_output(&f.cli().command).unwrap_err();
        assert_eq!(error.to_string(), "ssh_public_identity_invalid");
        assert!(!f.data.exists());
    }
}
