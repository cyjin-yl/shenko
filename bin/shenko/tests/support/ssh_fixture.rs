//! Public diagnostics and real OpenSSH support shared by CLI journeys.
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::{fs, io::Read, path::Path};

pub fn copy_diagnostics(logs: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(logs)? {
        let entry = entry?;
        // Only this explicit public-output directory, never the installation,
        // input files, private keys or private owner-control material.
        if entry.file_type()?.is_file() {
            fs::copy(entry.path(), destination.join(entry.file_name()))?;
        }
    }
    Ok(())
}

pub fn file_sha256(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub fn write_sshd_config(
    config: &Path,
    root: &Path,
    host_key: &Path,
    authorized_keys: &Path,
    port: u16,
) -> Result<()> {
    // Root-run container fixtures use PAM's account checks so a locked root
    // password does not disable independently authorized public keys.
    // SAFETY: geteuid only observes this process's effective identity.
    let use_pam = if unsafe { libc::geteuid() } == 0 {
        "yes"
    } else {
        "no"
    };
    fs::write(
        config,
        format!(
            "ListenAddress 127.0.0.1\nPort {port}\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\n\
         PasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM {use_pam}\nStrictModes no\n\
         PermitRootLogin yes\nSubsystem sftp internal-sftp\nLogLevel VERBOSE\n",
            host_key.display(),
            root.join("pid").display(),
            authorized_keys.display(),
        ),
    )?;
    Ok(())
}
