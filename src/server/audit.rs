//! Persistent JSON-lines audit log in the private admin directory.
//!
//! Entries never contain secrets, vectors, payloads or metadata values.
//! The file rotates to `audit.1.jsonl` at 16 MiB, replacing the previous
//! rotated file, so at most two files (32 MiB) exist.
use super::*;
use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path as FilePath, PathBuf},
};

const MAX_BYTES: u64 = 16 * 1024 * 1024;
const TAIL_BYTES: u64 = 4 * 1024 * 1024;
const DENIAL_WINDOW_MS: u128 = 60_000;
const MAX_DENIAL_KEYS: usize = 4096;

pub(super) struct Log {
    path: PathBuf,
    rotated: PathBuf,
    max_bytes: u64,
    /// The current handle and its size. `None` only after a failed
    /// rotation that also failed to reopen; the next append retries.
    file: Mutex<(Option<File>, u64)>,
    denials: Mutex<HashMap<(String, String), u128>>,
}

fn open_file(path: &FilePath) -> std::io::Result<(File, u64)> {
    if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(std::io::Error::other("audit log must not be a symlink"));
    }
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let size = file.metadata()?.len();
    Ok((file, size))
}

impl Log {
    pub(super) fn open(directory: &FilePath) -> std::io::Result<Self> {
        Self::open_with_limit(directory, MAX_BYTES)
    }

    fn open_with_limit(directory: &FilePath, max_bytes: u64) -> std::io::Result<Self> {
        let path = directory.join("audit.jsonl");
        let (file, size) = open_file(&path)?;
        Ok(Self {
            rotated: directory.join("audit.1.jsonl"),
            path,
            max_bytes,
            file: Mutex::new((Some(file), size)),
            denials: Mutex::new(HashMap::new()),
        })
    }

    /// Appends one event with a timestamp. `sync` makes it durable before
    /// returning (credential, backup and retention events).
    pub(super) fn append(&self, mut event: Value, sync: bool) -> std::io::Result<()> {
        event["unix_ms"] = json!(admin::now_ms() as u64);
        let mut line = serde_json::to_vec(&event).map_err(std::io::Error::other)?;
        line.push(b'\n');
        let mut guard = self
            .file
            .lock()
            .map_err(|_| std::io::Error::other("audit lock poisoned"))?;
        if guard.0.is_none() || guard.1 + line.len() as u64 > self.max_bytes {
            self.rotate(&mut guard)?;
        }
        let (Some(file), size) = &mut *guard else {
            return Err(std::io::Error::other("audit log unavailable"));
        };
        file.write_all(&line)?;
        *size += line.len() as u64;
        if sync {
            file.sync_data()?;
        }
        Ok(())
    }

    /// Bounded rotation that works where rename cannot replace an existing
    /// or open file (Windows): sync and close the current handle, remove
    /// the previous rotated file (never following a symlink), rename, then
    /// reopen. On any failure the current file is reopened for appending
    /// (so the log keeps working and the size check retries), and the error
    /// is returned so callers that require a durable audit fail closed.
    fn rotate(&self, guard: &mut (Option<File>, u64)) -> std::io::Result<()> {
        let size = guard.1;
        if size == 0 && guard.0.is_some() {
            return Ok(());
        }
        let moved = (|| {
            if let Some(file) = guard.0.take() {
                file.sync_all()?;
            }
            if size > 0 {
                match std::fs::symlink_metadata(&self.rotated) {
                    Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
                        return Err(std::io::Error::other(
                            "rotated audit log must be a regular file",
                        ));
                    }
                    Ok(_) => std::fs::remove_file(&self.rotated)?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                std::fs::rename(&self.path, &self.rotated)?;
            }
            Ok(())
        })();
        *guard = match open_file(&self.path) {
            Ok((file, size)) => (Some(file), size),
            Err(error) => {
                *guard = (None, 0);
                return Err(moved.err().unwrap_or(error));
            }
        };
        moved
    }

    /// Rate limit for repeated scope denials: one entry per credential and
    /// route per minute (the counter still counts every denial).
    pub(super) fn should_record_denial(&self, credential: &str, route: &str) -> bool {
        let Ok(mut denials) = self.denials.lock() else {
            return false;
        };
        let now = admin::now_ms();
        if denials.len() >= MAX_DENIAL_KEYS {
            denials.retain(|_, at| now.saturating_sub(*at) < DENIAL_WINDOW_MS);
            if denials.len() >= MAX_DENIAL_KEYS {
                return false;
            }
        }
        let key = (credential.to_owned(), route.to_owned());
        match denials.get(&key) {
            Some(at) if now.saturating_sub(*at) < DENIAL_WINDOW_MS => false,
            _ => {
                denials.insert(key, now);
                true
            }
        }
    }

    /// Newest-last tail of at most `limit` entries from the current file.
    pub(super) fn tail(&self, limit: usize) -> std::io::Result<Vec<Value>> {
        let _guard = self
            .file
            .lock()
            .map_err(|_| std::io::Error::other("audit lock poisoned"))?;
        let mut file = File::open(&self.path)?;
        let size = file.metadata()?.len();
        let start = size.saturating_sub(TAIL_BYTES);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(TAIL_BYTES).read_to_end(&mut bytes)?;
        let text = String::from_utf8_lossy(&bytes);
        let mut lines = text.lines().collect::<Vec<_>>();
        if start > 0 && !lines.is_empty() {
            lines.remove(0);
        }
        let skip = lines.len().saturating_sub(limit);
        Ok(lines
            .into_iter()
            .skip(skip)
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect())
    }
}

/// The stable, non-secret identity that audit events record as `actor`:
///
/// * `user:<account id>`: a super-admin dashboard session. A signed-in editor
///   or viewer is a scoped grant whose id is already `user:<account id>`;
/// * `admin_token`: the bootstrap admin token and, without a configured admin
///   token, every caller of the local server (no session principal exists);
/// * `read_only_token`: the read-only token;
/// * the credential id: a scoped credential (12 characters of `a-z2-7`, so it
///   never equals another encoding).
///
/// Only the account id is read from a session principal. Cookies, session
/// keys, tokens, secrets, user names and request content never enter it.
pub(super) fn actor(role: &admin::Role, principal: Option<&users::Principal>) -> String {
    match (role, principal) {
        (admin::Role::Admin, Some(principal)) => format!("user:{}", principal.user_id),
        (admin::Role::Admin, None) => "admin_token".to_owned(),
        (admin::Role::ReadOnly, _) => "read_only_token".to_owned(),
        (admin::Role::Scoped(grant), _) => grant.id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(path: &FilePath) -> Vec<u64> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["n"]
                    .as_u64()
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn repeated_rotation_replaces_the_previous_rotated_file_and_stays_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let log = Log::open_with_limit(dir.path(), 200).unwrap();
        for n in 0..40u64 {
            log.append(json!({"event": "test", "n": n}), n % 2 == 0)
                .unwrap_or_else(|error| panic!("append {n}: {error}"));
        }
        let current = lines(&dir.path().join("audit.jsonl"));
        let rotated = lines(&dir.path().join("audit.1.jsonl"));
        assert_eq!(current.last(), Some(&39));
        // Several rotations happened; the newest rotated file directly
        // precedes the current one and nothing was lost between them.
        assert!(rotated.len() >= 2 && !current.is_empty());
        assert_eq!(rotated.last().unwrap() + 1, current[0]);
        assert!(rotated[0] > 0, "older rotated content was replaced");
        let files = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(files, 2, "bounded: current plus one rotated file");
        assert!(
            std::fs::metadata(dir.path().join("audit.jsonl"))
                .unwrap()
                .len()
                <= 200
        );
        let tail = log.tail(3).unwrap();
        assert_eq!(tail.last().unwrap()["n"], 39);
        // Reopening continues the bounded scheme.
        drop(log);
        let log = Log::open_with_limit(dir.path(), 200).unwrap();
        for n in 40..60u64 {
            log.append(json!({"event": "test", "n": n}), true).unwrap();
        }
        assert_eq!(lines(&dir.path().join("audit.jsonl")).last(), Some(&59));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_rotated_file_fails_closed_and_logging_continues_after_fix() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("elsewhere");
        std::fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("audit.1.jsonl")).unwrap();
        let log = Log::open_with_limit(dir.path(), 100).unwrap();
        let mut failed = false;
        for n in 0..10u64 {
            failed |= log.append(json!({"event": "test", "n": n}), true).is_err();
        }
        assert!(failed);
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        std::fs::remove_file(dir.path().join("audit.1.jsonl")).unwrap();
        log.append(json!({"event": "test", "n": 99}), true).unwrap();
    }

    const ACCOUNT: &str = "0123456789abcdef0123456789abcdef";
    const OTHER_ACCOUNT: &str = "fedcba9876543210fedcba9876543210";
    const SESSION_KEY: [u8; 32] = *b"SESSIONKEYSESSIONKEYSESSIONKEY12";

    fn signed_in(account: &str, username: &str, role: users::AccountRole) -> users::Principal {
        users::Principal::for_test(account, username, role, SESSION_KEY)
    }

    /// A grant built like a signed-in user's (`id` is `user:<account id>`) or,
    /// with a 12-character `a-z2-7` id, like a stored credential's.
    fn scoped(id: &str) -> admin::Role {
        admin::Role::Scoped(Arc::new(credentials::Grant::for_user(
            id.to_owned(),
            "display name".to_owned(),
            None,
            &[credentials::Op::Read],
            1,
            u64::MAX,
        )))
    }

    #[test]
    fn actor_names_every_role_and_principal_combination() {
        let (admin_token, read_only) = (admin::Role::Admin, admin::Role::ReadOnly);
        let credential = scoped("abcdefghij27");
        let editor = scoped(&format!("user:{OTHER_ACCOUNT}"));
        let root = signed_in(ACCOUNT, "alice.smith", users::AccountRole::SuperAdmin);
        let eddie = signed_in(OTHER_ACCOUNT, "eddie", users::AccountRole::Editor);
        let cases = [
            // The bootstrap admin token and insecure local mode: no session.
            (&admin_token, None, "admin_token".to_owned()),
            // A super-admin dashboard session.
            (&admin_token, Some(&root), format!("user:{ACCOUNT}")),
            (&read_only, None, "read_only_token".to_owned()),
            // The read-only token has no session; a principal is ignored.
            (&read_only, Some(&root), "read_only_token".to_owned()),
            (&credential, None, "abcdefghij27".to_owned()),
            (&credential, Some(&root), "abcdefghij27".to_owned()),
            // A signed-in editor: its grant id is already the account encoding.
            (&editor, None, format!("user:{OTHER_ACCOUNT}")),
            (&editor, Some(&eddie), format!("user:{OTHER_ACCOUNT}")),
            (&editor, Some(&root), format!("user:{OTHER_ACCOUNT}")),
        ];
        for (role, session, expected) in &cases {
            assert_eq!(&actor(role, *session), expected, "{}", role.name());
        }
        // The encodings never collide: a credential id is 12 characters of
        // `a-z2-7`, which none of the other three forms can be.
        let credential_id = |text: &str| {
            text.len() == 12 && text.bytes().all(|b| matches!(b, b'a'..=b'z' | b'2'..=b'7'))
        };
        for (_, _, expected) in &cases {
            assert_eq!(credential_id(expected), expected == "abcdefghij27");
        }
    }

    #[test]
    fn a_session_actor_is_the_account_id_and_nothing_else() {
        let first = signed_in(ACCOUNT, "alice.smith", users::AccountRole::SuperAdmin);
        // The same account in a later session: another key, a renamed user, a
        // different expiry and password state.
        let mut later =
            users::Principal::for_test(ACCOUNT, "renamed", users::AccountRole::SuperAdmin, [7; 32]);
        later.expires = 1;
        later.must_change_password = true;
        let actor_of = |session: &users::Principal| actor(&admin::Role::Admin, Some(session));
        assert_eq!(actor_of(&first), format!("user:{ACCOUNT}"));
        assert_eq!(actor_of(&first), actor_of(&later));
        let other = signed_in(OTHER_ACCOUNT, "alice.smith", users::AccountRole::SuperAdmin);
        assert_eq!(actor_of(&other), format!("user:{OTHER_ACCOUNT}"));
        assert_ne!(actor_of(&first), actor_of(&other));
        // Neither the user name nor any rendering of the session key appears.
        let hex: String = SESSION_KEY.iter().map(|b| format!("{b:02x}")).collect();
        let standard = BASE64_STANDARD.encode(SESSION_KEY);
        let url_safe = base64::prelude::BASE64_URL_SAFE_NO_PAD.encode(SESSION_KEY);
        let text = actor_of(&first);
        for secret in [
            "alice.smith",
            "alice",
            "SESSIONKEY",
            hex.as_str(),
            standard.as_str(),
            url_safe.as_str(),
        ] {
            assert!(!text.contains(secret), "{text} contains {secret}");
        }
    }
}
