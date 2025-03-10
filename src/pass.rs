/*  Ripasso - a simple password manager
    Copyright (C) 2019-2020 Joakim Lundborg, Alexander Kjäll

    This program is free software: you can redistribute it and/or modify
    it under the terms of the GNU General Public License as published by
    the Free Software Foundation, either version 3 of the License, or
    (at your option) any later version.

    This program is distributed in the hope that it will be useful,
    but WITHOUT ANY WARRANTY; without even the implied warranty of
    MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
    GNU General Public License for more details.

    You should have received a copy of the GNU General Public License
    along with this program.  If not, see <https://www.gnu.org/licenses/>.
*/

use std::fs;
use std::fs::{create_dir_all, File};
use std::path::{Path, PathBuf};
use std::str;

use chrono::prelude::*;
use std::io::prelude::*;
use std::sync::{Arc, Mutex};

use git2::{Oid, Repository};

use crate::crypto::{Crypto, FindSigningFingerprintStrategy, GpgMe, VerificationError};
pub use crate::error::{Error, Result};
pub use crate::signature::{
    parse_signing_keys, Comment, KeyRingStatus, OwnerTrustLevel, Recipient, SignatureStatus,
};
use std::collections::HashMap;
use std::fmt::Display;

/// Represents a complete password store directory
pub struct PasswordStore {
    /// Name given to the store in a config file
    name: String,
    /// The absolute path to the root directory of the password store
    root: PathBuf,
    /// A list of fingerprints of keys that are allowed to sign the .gpg-id file, obtained from the environmental
    /// variable `PASSWORD_STORE_SIGNING_KEY` or from the configuration file
    valid_gpg_signing_keys: Vec<[u8; 20]>,
    /// a list of password files with meta data
    pub passwords: Vec<PasswordEntry>,
    /// A file that describes the style of the store
    style_file: Option<PathBuf>,
    /// The gpg implementation
    crypto: Box<dyn Crypto + Send>,
}

impl PasswordStore {
    /// Constructs a `PasswordStore` object. If `password_store_signing_key` is present,
    /// the function verifies that the .gpg-id file is signed correctly
    pub fn new(
        store_name: &str,
        password_store_dir: &Option<PathBuf>,
        password_store_signing_key: &Option<String>,
        home: &Option<PathBuf>,
        style_file: &Option<PathBuf>,
    ) -> Result<PasswordStore> {
        let pass_home = password_dir_raw(password_store_dir, home);
        if !pass_home.exists() {
            return Err(Error::Generic("failed to locate password directory"));
        }

        let crypto = Box::new(GpgMe {});

        let signing_keys = parse_signing_keys(password_store_signing_key, crypto.as_ref())?;

        let store = PasswordStore {
            name: store_name.to_owned(),
            root: pass_home.canonicalize()?,
            valid_gpg_signing_keys: signing_keys,
            passwords: [].to_vec(),
            style_file: style_file.to_owned(),
            crypto,
        };

        if !store.valid_gpg_signing_keys.is_empty() {
            store.verify_gpg_id_file(&pass_home, &store.valid_gpg_signing_keys)?;
        }

        Ok(store)
    }

    /// Creates a `PasswordStore`
    pub fn create(
        store_name: &str,
        password_store_dir: &Option<PathBuf>,
        recipients: &[Recipient],
        recipients_as_signers: bool,
        home: &Option<PathBuf>,
        style_file: &Option<PathBuf>,
    ) -> Result<PasswordStore> {
        let pass_home = password_dir_raw(password_store_dir, home);
        if pass_home.exists() {
            return Err(Error::Generic(
                "trying to create a pass store in an existing directory",
            ));
        }

        if recipients.is_empty() {
            return Err(Error::Generic(
                "password store must have at least one member",
            ));
        }
        for recipient in recipients {
            if recipient.key_id.len() != 40 && recipient.key_id.len() != 42 {
                return Err(Error::Generic(
                    "member specification wasn't a full pgp fingerprint",
                ));
            }
        }

        let crypto = Box::new(GpgMe {});

        let signing_keys = {
            if recipients_as_signers {
                let mut fingerprints = vec![];
                for r in recipients {
                    if r.fingerprint.is_none() {
                        return Err(Error::GenericDyn(format!(
                            "recipient {} ({}) doesn't have a fingerprint",
                            r.name, r.key_id
                        )));
                    }
                    fingerprints.push(r.fingerprint.unwrap())
                }
                fingerprints
            } else {
                vec![]
            }
        };

        create_dir_all(&pass_home)?;
        Recipient::write_recipients_file(
            recipients,
            &pass_home.join(".gpg-id"),
            &signing_keys,
            crypto.as_ref(),
        )?;
        let repo = init_git_repo(&pass_home)?;

        if recipients_as_signers {
            add_and_commit_internal(
                &repo,
                &[PathBuf::from(".gpg-id"), PathBuf::from(".gpg-id.sig")],
                "initial commit by Ripasso",
                crypto.as_ref(),
            )?;
        } else {
            add_and_commit_internal(
                &repo,
                &[PathBuf::from(".gpg-id")],
                "initial commit by Ripasso",
                crypto.as_ref(),
            )?;
        }

        let store = PasswordStore {
            name: store_name.to_owned(),
            root: pass_home.canonicalize()?,
            valid_gpg_signing_keys: signing_keys,
            passwords: [].to_vec(),
            style_file: style_file.to_owned(),
            crypto,
        };

        Ok(store)
    }

    /// Returns the name of the store, configured to the configuration file
    pub fn get_name(&self) -> &String {
        &self.name
    }

    /// Returns a vec with the keys that are allowed to sign the .gpg-id file
    pub fn get_valid_gpg_signing_keys(&self) -> &Vec<[u8; 20]> {
        &self.valid_gpg_signing_keys
    }

    /// returns the path to the directory where the store is located.
    pub fn get_store_path(&self) -> PathBuf {
        self.root.clone()
    }

    /// returns the style file for the store
    pub fn get_style_file(&self) -> Option<PathBuf> {
        self.style_file.clone()
    }

    fn repo(&self) -> Result<git2::Repository> {
        git2::Repository::open(&self.root).map_err(Error::Git)
    }

    fn verify_gpg_id_file(
        &self,
        pass_home: &Path,
        signing_keys: &[[u8; 20]],
    ) -> Result<SignatureStatus> {
        let mut gpg_id_file = pass_home.to_path_buf();
        gpg_id_file.push(".gpg-id");
        let mut gpg_id_sig_file = pass_home.to_path_buf();
        gpg_id_sig_file.push(".gpg-id.sig");

        let gpg_id = fs::read(gpg_id_file)?;
        let gpg_id_sig = match fs::read(gpg_id_sig_file) {
            Ok(c) => c,
            Err(_) => {
                return Err(Error::Generic(
                    "problem reading .gpg-id.sig, and strict signature checking was asked for",
                ))
            }
        };

        match self.crypto.verify_sign(&gpg_id, &gpg_id_sig, signing_keys) {
            Ok(r) => Ok(r),
            Err(VerificationError::InfrastructureError(message)) => Err(Error::GenericDyn(message)),
            Err(VerificationError::SignatureFromWrongRecipient) => Err(Error::Generic("the .gpg-id file wasn't signed by one of the keys specified in the environmental variable PASSWORD_STORE_SIGNING_KEY")),
            Err(VerificationError::BadSignature) => Err(Error::Generic("Bad signature for .gpg-id file")),
            Err(VerificationError::MissingSignatures) => Err(Error::Generic("Missing signature for .gpg-id file, and PASSWORD_STORE_SIGNING_KEY specified")),
            Err(VerificationError::TooManySignatures) => Err(Error::Generic("Signature for .gpg-id file contained more than one signature, something is fishy")),
        }
    }

    /// Creates a new password file in the store.
    pub fn new_password_file(&mut self, path_end: &str, content: &str) -> Result<PasswordEntry> {
        let mut path = self.root.clone();

        let c_path = std::fs::canonicalize(path.as_path())?;

        let path_iter = &mut path_end.split('/').peekable();

        while let Some(p) = path_iter.next() {
            if path_iter.peek().is_some() {
                path.push(p);
                let c_file_res = std::fs::canonicalize(path.as_path());
                if let Ok(c_file) = c_file_res {
                    if !c_file.starts_with(c_path.as_path()) {
                        return Err(Error::Generic(
                            "trying to write outside of password store directory",
                        ));
                    }
                }
                if !path.exists() {
                    std::fs::create_dir(&path)?;
                }
            } else {
                path.push(format!("{}.gpg", p));
            }
        }

        if path.exists() {
            return Err(Error::Generic("file already exist"));
        }

        match self.new_password_file_internal(&path, path_end, content) {
            Ok(pe) => Ok(pe),
            Err(err) => {
                // try to remove the file we created, as cleanup
                let _ = std::fs::remove_file(path);

                // but always return the original error
                Err(err)
            }
        }
    }

    fn new_password_file_internal(
        &mut self,
        path: &Path,
        path_end: &str,
        content: &str,
    ) -> Result<PasswordEntry> {
        let mut file = File::create(&path)?;

        if !self.valid_gpg_signing_keys.is_empty() {
            self.verify_gpg_id_file(&self.root, &self.valid_gpg_signing_keys)?;
        }

        let mut recipients_file = self.root.clone();
        recipients_file.push(".gpg-id");
        let recipients = self.all_recipients()?;
        let output = self.crypto.encrypt_string(content, &recipients)?;

        if let Err(why) = file.write_all(&output) {
            return Err(Error::from(why));
        }
        match self.repo() {
            Err(_) => {
                self.passwords.push(PasswordEntry::load_from_filesystem(
                    &self.root,
                    &append_extension(PathBuf::from(path_end), ".gpg"),
                )?);
                PasswordEntry::load_from_filesystem(
                    &self.root,
                    &append_extension(PathBuf::from(path_end), ".gpg"),
                )
            }
            Ok(repo) => {
                let message = format!("Add password for {} using ripasso", path_end);

                add_and_commit_internal(
                    &repo,
                    &[append_extension(PathBuf::from(path_end), ".gpg")],
                    &message,
                    self.crypto.as_ref(),
                )?;

                self.passwords
                    .push(PasswordEntry::load_from_git(&self.root, path, &repo, self));

                Ok(PasswordEntry::load_from_git(&self.root, path, &repo, self))
            }
        }
    }

    /// loads the list of passwords from disk again
    pub fn reload_password_list(&mut self) -> Result<()> {
        let mut new_passwords = self.all_passwords()?;

        self.passwords.clear();

        self.passwords.append(&mut new_passwords);

        Ok(())
    }

    /// checks if there is a user name configured in git
    pub fn has_configured_username(&self) -> bool {
        if self.repo().is_err() {
            return true;
        }

        match git2::Config::open_default() {
            Err(_) => false,
            Ok(config) => {
                let user_name = config.get_string("user.name");

                if user_name.is_err() {
                    return false;
                }
                true
            }
        }
    }

    /// Read the password store directory and return a list of all the password files.
    pub fn all_passwords(&self) -> Result<Vec<PasswordEntry>> {
        let mut passwords = vec![];
        let repo = self.repo();

        // Not a git repository
        if repo.is_err() {
            let password_path_glob = self.root.join("**/*.gpg");
            let existing_iter = glob::glob(&password_path_glob.to_string_lossy())?;

            for existing_file in existing_iter {
                let relpath = existing_file?.strip_prefix(&self.root)?.to_path_buf();
                passwords.push(PasswordEntry::load_from_filesystem(&self.root, &relpath)?);
            }

            return Ok(passwords);
        }

        let repo = repo?;

        // First, collect all files we need to find the first commit for
        let password_path_glob = self.root.join("**/*.gpg");
        let existing_iter = glob::glob(&password_path_glob.to_string_lossy())?;
        let mut files_to_find: Vec<PathBuf> = vec![];
        for existing_file in existing_iter {
            files_to_find.push(existing_file?.strip_prefix(&self.root)?.to_path_buf());
        }

        if files_to_find.is_empty() {
            return Ok(vec![]);
        }

        // Walk through all commits in reverse order, if the commit contains
        // the file, mark it
        let mut walk = repo.revwalk()?;
        walk.push(repo.head()?.target().ok_or("missing Oid on head")?)?;
        let mut last_tree = repo
            .find_commit(repo.head()?.target().ok_or("missing Oid on head")?)?
            .tree()?;
        let mut last_commit = repo.head()?.peel_to_commit()?;
        for rev in walk {
            if rev.is_err() {
                continue;
            }
            let oid = rev?;

            let commit = repo.find_commit(oid)?;
            let tree = commit.tree()?;

            let diff = repo.diff_tree_to_tree(Some(&last_tree), Some(&tree), None)?;

            diff.foreach(
                &mut |delta: git2::DiffDelta, _f: f32| {
                    if let Some(found) = delta.new_file().path() {
                        files_to_find.retain(|target| {
                            push_password_if_match(
                                target,
                                found,
                                &commit,
                                &repo,
                                &mut passwords,
                                &oid,
                                self,
                            )
                        });
                    }
                    true
                },
                None,
                None,
                None,
            )?;

            last_tree = tree;
            last_commit = commit;
        }

        // When we have checked all the diffs, we also need to consider what
        // files was checked in to the first commit
        last_tree.walk(git2::TreeWalkMode::PreOrder, |path, entry| {
            if let Some(entry_name) = entry.name() {
                let found = Path::new(path).join(entry_name);
                files_to_find.retain(|target| {
                    push_password_if_match(
                        target,
                        &found,
                        &last_commit,
                        &repo,
                        &mut passwords,
                        &last_commit.id(),
                        self,
                    )
                });
            }
            git2::TreeWalkResult::Ok
        })?;

        // If there are any files we couldn't find, add them to the list anyway
        for not_found in files_to_find {
            passwords.push(PasswordEntry::new(
                &self.root,
                &not_found.to_path_buf(),
                Err(Error::Generic("")),
                Err(Error::Generic("")),
                Err(Error::Generic("")),
                RepositoryStatus::NotInRepo,
            ));
        }

        Ok(passwords)
    }

    /// Return a list of all the Recipients in the `$PASSWORD_STORE_DIR/.gpg-id` file.
    pub fn all_recipients(&self) -> Result<Vec<Recipient>> {
        if !self.valid_gpg_signing_keys.is_empty() {
            self.verify_gpg_id_file(&self.root, &self.valid_gpg_signing_keys)?;
        }

        Recipient::all_recipients(&self.recipients_file(), self.crypto.as_ref())
    }

    fn recipients_file(&self) -> PathBuf {
        let mut rf = self.root.clone();
        rf.push(".gpg-id");
        rf
    }

    /// Removes a key from the .gpg-id file and re-encrypts all the passwords
    pub fn remove_recipient(&self, r: &Recipient) -> Result<()> {
        Recipient::remove_recipient_from_file(
            r,
            &self.recipients_file(),
            &self.valid_gpg_signing_keys,
            self.crypto.as_ref(),
        )?;
        self.reencrypt_all_password_entries()
    }

    /// Adds a key to the .gpg-id file and re-encrypts all the passwords
    pub fn add_recipient(&self, r: &Recipient) -> Result<()> {
        Recipient::add_recipient_to_file(
            r,
            &self.recipients_file(),
            &self.valid_gpg_signing_keys,
            self.crypto.as_ref(),
        )?;
        self.reencrypt_all_password_entries()
    }

    /// Reencrypt all the entries in the store, for example when a new collaborator is added
    /// to the team.
    fn reencrypt_all_password_entries(&self) -> Result<()> {
        let mut names: Vec<PathBuf> = Vec::new();
        for entry in self.all_passwords()? {
            entry.update_internal(entry.secret(self)?, self)?;
            names.push(append_extension(PathBuf::from(&entry.name), ".gpg"));
        }
        names.push(PathBuf::from(".gpg-id"));

        if self.repo().is_err() {
            return Ok(());
        }

        let keys = self
            .all_recipients()?
            .into_iter()
            .map(|s| format!("0x{}, ", s.key_id))
            .collect::<String>();
        let message = format!("Reencrypt password store with new GPG ids {}", keys);

        self.add_and_commit(&names, &message)?;

        Ok(())
    }

    /// Add a file to the store, and commit it to the supplied git repository.
    pub fn add_and_commit(&self, paths: &[PathBuf], message: &str) -> Result<git2::Oid> {
        let repo = self.repo();
        if repo.is_err() {
            return Err(Error::Generic("must have a repository"));
        }
        let repo = repo?;

        let mut index = repo.index()?;
        for path in paths {
            index.add_path(path)?;
        }
        let oid = index.write_tree()?;
        let signature = repo.signature()?;
        let parent_commit_res = find_last_commit(&repo);
        let mut parents = vec![];
        let parent_commit;
        if parent_commit_res.is_ok() {
            parent_commit = parent_commit_res?;
            parents.push(&parent_commit);
        }
        let tree = repo.find_tree(oid)?;

        let oid = commit(
            &repo,
            &signature,
            message,
            &tree,
            &parents,
            self.crypto.as_ref(),
        )?;
        let obj = repo.find_object(oid, None)?;
        repo.reset(&obj, git2::ResetType::Hard, None)?;

        Ok(oid)
    }

    ///Renames a password file to a new name
    ///returns the index in the password vec of the renamed PasswordEntry
    pub fn rename_file(&mut self, old_name: &str, new_name: &str) -> Result<usize> {
        if new_name.starts_with('/') || new_name.contains("..") {
            return Err(Error::Generic("directory traversal not allowed"));
        }

        let mut old_path = self.root.clone();
        old_path.push(PathBuf::from(old_name));
        let old_path = append_extension(old_path, ".gpg");
        let mut new_path = self.root.clone();
        new_path.push(PathBuf::from(new_name));
        let new_path = append_extension(new_path, ".gpg");

        if !old_path.exists() {
            return Err(Error::Generic("source file is missing"));
        }

        if new_path.exists() {
            return Err(Error::Generic("can't target file already exists"));
        }

        let mut new_path_dir = new_path.clone();
        new_path_dir.pop();
        fs::create_dir_all(&new_path_dir)?;

        fs::rename(&old_path, &new_path)?;

        if self.repo().is_ok() {
            let old_file_name = append_extension(PathBuf::from(old_name), ".gpg");
            let new_file_name = append_extension(PathBuf::from(new_name), ".gpg");
            move_and_commit(self, &old_file_name, &new_file_name, "moved file")?;
        }

        let passwords = &mut self.passwords;
        let mut index = usize::MAX;
        for (i, entry) in passwords.iter().enumerate() {
            if entry.name == old_name {
                index = i;
            }
        }
        if index != usize::MAX {
            let old_entry = passwords.swap_remove(index);
            let relpath = new_path
                .strip_prefix(&self.root)
                .expect("base was not a prefix of path")
                .to_path_buf();
            let new_entry = PasswordEntry::with_new_name(old_entry, &self.root, &relpath);
            passwords.push(new_entry);
        }

        Ok(passwords.len() - 1)
    }

    pub fn recipient_from(
        &self,
        key_id: &str,
        pre_comment: &[String],
        post_comment: Option<String>,
    ) -> Result<Recipient> {
        crate::signature::Recipient::from(key_id, pre_comment, post_comment, self.crypto.as_ref())
    }
}

pub fn all_recipients_from_stores(
    stores: Arc<Mutex<Vec<Arc<Mutex<PasswordStore>>>>>,
) -> Result<Vec<Recipient>> {
    let all_recipients: Vec<Recipient> = {
        let mut ar: HashMap<String, Recipient> = HashMap::new();
        let stores = stores.lock().unwrap();
        for store in stores.iter() {
            let store = store.lock().unwrap();
            for recipient in store.all_recipients()? {
                let key = {
                    if recipient.fingerprint == None {
                        recipient.key_id.clone()
                    } else {
                        hex::encode_upper(recipient.fingerprint.as_ref().unwrap())
                    }
                };
                ar.insert(key, recipient);
            }
        }
        ar.into_iter().map(|(_id, r)| r).collect()
    };

    Ok(all_recipients)
}

fn push_password_if_match(
    target: &Path,
    found: &Path,
    commit: &git2::Commit,
    repo: &git2::Repository,
    passwords: &mut Vec<PasswordEntry>,
    oid: &git2::Oid,
    store: &PasswordStore,
) -> bool {
    if *target == *found {
        let time = commit.time();
        let time_return = Ok(Local.timestamp(time.seconds(), 0));

        let name_return = name_from_commit(commit);

        let signature_return = verify_git_signature(repo, oid, store);

        passwords.push(PasswordEntry::new(
            &store.root,
            target,
            time_return,
            name_return,
            signature_return,
            RepositoryStatus::InRepo,
        ));
        return false;
    }
    true
}

/// Find the name of the committer, or an error message
fn name_from_commit(commit: &git2::Commit) -> Result<String> {
    match commit.committer().name() {
        Some(s) => Ok(s.to_owned()),
        None => Err(Error::Generic("missing committer name")),
    }
}

/// Describes one log line in the history of a file
pub struct GitLogLine {
    pub message: String,
    pub commit_time: DateTime<Local>,
    pub signature_status: Option<SignatureStatus>,
}

impl GitLogLine {
    pub fn new(
        message: String,
        commit_time: DateTime<Local>,
        signature_status: Option<SignatureStatus>,
    ) -> GitLogLine {
        GitLogLine {
            message,
            commit_time,
            signature_status,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RepositoryStatus {
    InRepo,
    NotInRepo,
    NoRepo,
}

/// One password in the password store
#[derive(Clone, Debug)]
pub struct PasswordEntry {
    /// Name of the entry, (from relative path to password)
    pub name: String,
    /// Absolute path to the password file
    path: PathBuf,
    /// if we have a git repo, then commit time
    pub updated: Option<DateTime<Local>>,
    /// if we have a git repo, then the name of the committer
    pub committed_by: Option<String>,
    /// if we have a git repo, and the commit was signed
    pub signature_status: Option<SignatureStatus>,
    /// describes if the file is in a repository or not
    pub is_in_git: RepositoryStatus,
}

fn to_name(relpath: &Path) -> String {
    let mut s = relpath.to_string_lossy().to_string();
    match s.ends_with(".gpg") {
        true => {
            s.truncate(s.len() - 4);
            s
        }
        false => s,
    }
}

impl PasswordEntry {
    /// constructs a a `PasswordEntry` from the supplied parts
    pub fn new(
        base: &Path,    // Root of the password directory
        relpath: &Path, // Relative path to the password.
        update_time: Result<DateTime<Local>>,
        committed_by: Result<String>,
        signature_status: Result<SignatureStatus>,
        is_in_git: RepositoryStatus,
    ) -> PasswordEntry {
        PasswordEntry {
            name: to_name(relpath),
            path: base.join(relpath),
            updated: update_time.ok(),
            committed_by: committed_by.ok(),
            signature_status: signature_status.ok(),
            is_in_git,
        }
    }

    /// Consumes an PasswordEntry, and returns a new one with a new name
    pub fn with_new_name(old: PasswordEntry, base: &Path, relpath: &Path) -> PasswordEntry {
        PasswordEntry {
            name: to_name(relpath),
            path: base.join(relpath),
            updated: old.updated,
            committed_by: old.committed_by,
            signature_status: old.signature_status,
            is_in_git: old.is_in_git,
        }
    }

    /// creates a `PasswordEntry` by running git blame on the specified path
    pub fn load_from_git(
        base: &Path,
        path: &Path,
        repo: &git2::Repository,
        store: &PasswordStore,
    ) -> PasswordEntry {
        let (update_time, committed_by, signature_status) =
            read_git_meta_data(base, path, repo, store);

        let relpath = path
            .strip_prefix(&base)
            .expect("base was not a prefix of path")
            .to_path_buf();
        PasswordEntry::new(
            base,
            &relpath,
            update_time,
            committed_by,
            signature_status,
            RepositoryStatus::InRepo,
        )
    }

    /// creates a `PasswordEntry` based on data in the filesystem
    pub fn load_from_filesystem(base: &Path, relpath: &Path) -> Result<PasswordEntry> {
        Ok(PasswordEntry {
            name: to_name(relpath),
            path: base.join(relpath),
            updated: None,
            committed_by: None,
            signature_status: None,
            is_in_git: RepositoryStatus::NoRepo,
        })
    }

    /// Decrypts and returns the full content of the PasswordEntry
    pub fn secret(&self, store: &PasswordStore) -> Result<String> {
        let s = fs::metadata(&self.path)?;
        if s.len() == 0 {
            return Err(Error::Generic("empty password file"));
        }

        let content = fs::read(&self.path)?;
        store.crypto.decrypt_string(&content)
    }

    /// Decrypts and returns the first line of the PasswordEntry
    pub fn password(&self, store: &PasswordStore) -> Result<String> {
        Ok(self.secret(store)?.split('\n').take(1).collect())
    }

    fn update_internal(&self, secret: String, store: &PasswordStore) -> Result<()> {
        if !store.valid_gpg_signing_keys.is_empty() {
            store.verify_gpg_id_file(&store.root, &store.valid_gpg_signing_keys)?;
        }

        let recipients = store.all_recipients()?;
        let ciphertext = store.crypto.encrypt_string(&secret, &recipients)?;

        let mut output = File::create(&self.path)?;
        output.write_all(&ciphertext)?;
        Ok(())
    }

    /// Updates the password store entry with new content, and commits those to git if a repository
    /// is supplied.
    pub fn update(&self, secret: String, store: &PasswordStore) -> Result<()> {
        self.update_internal(secret, store)?;

        if store.repo().is_err() {
            return Ok(());
        }

        let message = format!("Edit password for {} using ripasso", &self.name);

        store.add_and_commit(
            &[append_extension(PathBuf::from(&self.name), ".gpg")],
            &message,
        )?;

        Ok(())
    }

    /// Removes this entry from the filesystem and commit that to git if a repository is supplied.
    pub fn delete_file(&self, store: &PasswordStore) -> Result<()> {
        std::fs::remove_file(&self.path)?;

        if store.repo().is_err() {
            return Ok(());
        }
        let message = format!("Removed password file for {} using ripasso", &self.name);

        remove_and_commit(
            store,
            &[append_extension(PathBuf::from(&self.name), ".gpg")],
            &message,
        )?;
        Ok(())
    }

    /// Returns a list of log lines for the password, one line for each commit that have changed
    /// that password in some way
    pub fn get_history(&self, store: &PasswordStore) -> Result<Vec<GitLogLine>> {
        let repo = {
            let repo_res = store.repo();
            if repo_res.is_err() {
                return Ok(vec![]);
            }
            repo_res?
        };

        let mut revwalk = repo.revwalk()?;

        revwalk.set_sorting(git2::Sort::REVERSE)?;
        revwalk.set_sorting(git2::Sort::TIME)?;

        revwalk.push_head()?;

        let p = self.path.strip_prefix(&store.root)?;
        let ps = git2::Pathspec::new(vec![&p])?;

        let mut diffopts = git2::DiffOptions::new();
        diffopts.pathspec(&p);

        let walk_res: Vec<GitLogLine> = revwalk
            .filter_map(|id| {
                if let Ok(oid) = id {
                    if let Ok(commit) = repo.find_commit(oid) {
                        match commit.parents().len() {
                            0 => {
                                if let Ok(tree) = commit.tree() {
                                    let flags = git2::PathspecFlags::NO_MATCH_ERROR;
                                    ps.match_tree(&tree, flags).ok()?;
                                } else {
                                    return None;
                                }
                            }
                            _ => {
                                let m = commit.parents().all(|parent| {
                                    match_with_parent(&repo, &commit, &parent, &mut diffopts)
                                        .unwrap_or(false)
                                });
                                if !m {
                                    return None;
                                }
                            }
                        }

                        let time = commit.time();
                        let dt = Local.timestamp(time.seconds(), 0);

                        let signature_status = verify_git_signature(&repo, &oid, store);
                        Some(GitLogLine::new(
                            commit.message().unwrap_or("<no message>").to_owned(),
                            dt,
                            signature_status.ok(),
                        ))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();

        Ok(walk_res)
    }
}

/// returns true if the diff between the two commit's contains the path that the `DiffOptions`
/// have been prepared with
fn match_with_parent(
    repo: &git2::Repository,
    commit: &git2::Commit,
    parent: &git2::Commit,
    opts: &mut git2::DiffOptions,
) -> Result<bool> {
    let a = parent.tree()?;
    let b = commit.tree()?;
    let diff = repo.diff_tree_to_tree(Some(&a), Some(&b), Some(opts))?;
    Ok(diff.deltas().len() > 0)
}

fn find_last_commit(repo: &git2::Repository) -> Result<git2::Commit> {
    let obj = repo.head()?.resolve()?.peel(git2::ObjectType::Commit)?;
    obj.into_commit()
        .map_err(|_| Error::Generic("Couldn't find commit"))
}

/// Returns if a git commit should be gpg signed or not.
fn should_sign(repo: &git2::Repository) -> bool {
    match repo.config() {
        Err(_) => false,
        Ok(config) => config.get_bool("commit.gpgsign").unwrap_or(false),
    }
}

/// Apply the changes to the git repository.
fn commit(
    repo: &git2::Repository,
    signature: &git2::Signature,
    message: &str,
    tree: &git2::Tree,
    parents: &[&git2::Commit],
    crypto: &(dyn Crypto + Send),
) -> Result<git2::Oid> {
    if should_sign(repo) {
        let commit_buf = repo.commit_create_buffer(
            signature, // author
            signature, // committer
            message,   // commit message
            tree,      // tree
            parents,
        )?; // parents

        let commit_as_str = str::from_utf8(&commit_buf)?;

        let sig = crypto.sign_string(commit_as_str, &[], &FindSigningFingerprintStrategy::GIT)?;

        let commit = repo.commit_signed(commit_as_str, &sig, Some("gpgsig"))?;

        if let Ok(mut head) = repo.head() {
            head.set_target(commit, "added a signed commit using ripasso")?;
        } else {
            repo.branch("master", &repo.find_commit(commit)?, false)?;
        }

        Ok(commit)
    } else {
        let commit = repo.commit(
            Some("HEAD"), //  point HEAD to our new commit
            signature,    // author
            signature,    // committer
            message,      // commit message
            tree,         // tree
            parents,
        )?; // parents

        Ok(commit)
    }
}

/// Add a file to the store, and commit it to the supplied git repository.
fn add_and_commit_internal(
    repo: &git2::Repository,
    paths: &[PathBuf],
    message: &str,
    crypto: &(dyn Crypto + Send),
) -> Result<git2::Oid> {
    let mut index = repo.index()?;
    for path in paths {
        index.add_path(path)?;
        index.write()?;
    }
    let oid = index.write_tree()?;
    let signature = repo.signature()?;
    let parent_commit_res = find_last_commit(repo);
    let mut parents = vec![];
    let parent_commit;
    if parent_commit_res.is_ok() {
        parent_commit = parent_commit_res?;
        parents.push(&parent_commit);
    }
    index.write_tree()?;
    let tree = repo.find_tree(oid)?;

    let oid = commit(repo, &signature, message, &tree, &parents, crypto)?;

    Ok(oid)
}

/// Remove a file from the store, and commit the deletion to the supplied git repository.
fn remove_and_commit(store: &PasswordStore, paths: &[PathBuf], message: &str) -> Result<git2::Oid> {
    let repo = store
        .repo()
        .map_err(|_| Error::Generic("must have a repository"))?;

    let mut index = repo.index()?;
    for path in paths {
        index.remove_path(path)?;
        index.write()?;
    }
    let oid = index.write_tree()?;
    let signature = repo.signature()?;
    let parent_commit_res = find_last_commit(&repo);
    let mut parents = vec![];
    let parent_commit;
    if parent_commit_res.is_ok() {
        parent_commit = parent_commit_res?;
        parents.push(&parent_commit);
    }
    index.write_tree()?;
    let tree = repo.find_tree(oid)?;

    let oid = commit(
        &repo,
        &signature,
        message,
        &tree,
        &parents,
        store.crypto.as_ref(),
    )?;

    Ok(oid)
}

/// Move a file to a new place in the store, and commit the move to the supplied git repository.
fn move_and_commit(
    store: &PasswordStore,
    old_name: &Path,
    new_name: &Path,
    message: &str,
) -> Result<git2::Oid> {
    let repo = store
        .repo()
        .map_err(|_| Error::Generic("must have a repository"))?;

    let mut index = repo.index()?;
    index.remove_path(old_name)?;
    index.add_path(new_name)?;
    let oid = index.write_tree()?;
    let signature = repo.signature()?;
    let parent_commit_res = find_last_commit(&repo);
    let mut parents = vec![];
    let parent_commit;
    if parent_commit_res.is_ok() {
        parent_commit = parent_commit_res?;
        parents.push(&parent_commit);
    }
    let tree = repo.find_tree(oid)?;

    let oid = commit(
        &repo,
        &signature,
        message,
        &tree,
        &parents,
        store.crypto.as_ref(),
    )?;

    Ok(oid)
}

/// find the origin of the git repo, with the following strategy:
/// find the branch that HEAD points to, and read the remote configured for that branch
/// returns the remote and the name of the local branch
fn find_origin(repo: &git2::Repository) -> Result<(git2::Remote, String)> {
    for branch in repo.branches(Some(git2::BranchType::Local))? {
        let b = branch?.0;
        if b.is_head() {
            let upstream_name_buf = repo.branch_upstream_remote(&format!(
                "refs/heads/{}",
                &b.name()?.ok_or("no branch name")?
            ))?;
            let upstream_name = upstream_name_buf
                .as_str()
                .ok_or("Can't convert to string")?;
            let origin = repo.find_remote(upstream_name)?;
            return Ok((origin, b.name()?.ok_or("no branch name")?.to_owned()));
        }
    }

    Err(Error::Generic("no remotes configured"))
}

/// function that can be used for callback handling of the ssh interaction in git2
fn cred(
    tried_sshkey: &mut bool,
    _url: &str,
    username: Option<&str>,
    allowed: git2::CredentialType,
) -> std::result::Result<git2::Cred, git2::Error> {
    let sys_username = whoami::username();
    let user = match username {
        Some(name) => name,
        None => &sys_username,
    };

    if allowed.contains(git2::CredentialType::USERNAME) {
        return git2::Cred::username(user);
    }

    if *tried_sshkey {
        return Err(git2::Error::from_str("no authentication available"));
    }
    *tried_sshkey = true;

    git2::Cred::ssh_key_from_agent(user)
}

/// Push your changes to the remote git repository.
pub fn push(store: &PasswordStore) -> Result<()> {
    let repo = store
        .repo()
        .map_err(|_| Error::Generic("must have a repository"))?;

    let mut ref_status = None;
    let (mut origin, branch_name) = find_origin(&repo)?;
    let res = {
        let mut callbacks = git2::RemoteCallbacks::new();
        let mut tried_ssh_key = false;
        callbacks.credentials(|_url, username, allowed| {
            cred(&mut tried_ssh_key, _url, username, allowed)
        });
        callbacks.push_update_reference(|refname, status| {
            assert_eq!(refname, format!("refs/heads/{}", branch_name));
            ref_status = status.map(|s| s.to_owned());
            Ok(())
        });
        let mut opts = git2::PushOptions::new();
        opts.remote_callbacks(callbacks);
        origin.push(&[format!("refs/heads/{}", branch_name)], Some(&mut opts))
    };
    match res {
        Ok(()) if ref_status.is_none() => Ok(()),
        Ok(()) => Err(Error::GenericDyn(format!(
            "failed to push a ref: {:?}",
            ref_status
        ))),
        Err(e) => Err(Error::GenericDyn(format!("failure to push: {}", e))),
    }
}

/// Pull new changes from the remote git repository.
pub fn pull(store: &PasswordStore) -> Result<()> {
    let repo = store
        .repo()
        .map_err(|_| Error::Generic("must have a repository"))?;

    let (mut origin, branch_name) = find_origin(&repo)?;

    let mut cb = git2::RemoteCallbacks::new();
    let mut tried_ssh_key = false;
    cb.credentials(|_url, username, allowed| cred(&mut tried_ssh_key, _url, username, allowed));

    let mut opts = git2::FetchOptions::new();
    opts.remote_callbacks(cb);
    origin.fetch(&[branch_name], Some(&mut opts), None)?;

    let remote_oid = repo.refname_to_id("FETCH_HEAD")?;
    let head_oid = repo.refname_to_id("HEAD")?;

    let (_, behind) = repo.graph_ahead_behind(head_oid, remote_oid)?;

    if behind == 0 {
        return Ok(());
    }

    let remote_annotated_commit = repo.find_annotated_commit(remote_oid)?;
    let remote_commit = repo.find_commit(remote_oid)?;
    repo.merge(&[&remote_annotated_commit], None, None)?;

    //commit it
    let mut index = repo.index()?;
    let oid = index.write_tree()?;
    let signature = repo.signature()?;
    let parent_commit = find_last_commit(&repo)?;
    let tree = repo.find_tree(oid)?;
    let message = "pull and merge by ripasso";
    let _commit = repo.commit(
        Some("HEAD"), //  point HEAD to our new commit
        &signature,   // author
        &signature,   // committer
        message,      // commit message
        &tree,        // tree
        &[&parent_commit, &remote_commit],
    )?; // parents

    //cleanup
    repo.cleanup_state()?;
    Ok(())
}

/// Import the key_ids from the signature file from a keyserver.
pub fn pgp_pull(store: &PasswordStore) -> Result<String> {
    let recipients = store.all_recipients()?;

    let result = store.crypto.pull_keys(&recipients)?;

    Ok(result)
}

pub fn pgp_import(store: &PasswordStore, text: &str) -> Result<String> {
    store.crypto.import_key(text)
}

fn triple<T: Display>(
    e: &T,
) -> (
    Result<DateTime<Local>>,
    Result<String>,
    Result<SignatureStatus>,
) {
    (
        Err(Error::GenericDyn(format!("{}", e))),
        Err(Error::GenericDyn(format!("{}", e))),
        Err(Error::GenericDyn(format!("{}", e))),
    )
}

fn read_git_meta_data(
    base: &Path,
    path: &Path,
    repo: &git2::Repository,
    store: &PasswordStore,
) -> (
    Result<DateTime<Local>>,
    Result<String>,
    Result<SignatureStatus>,
) {
    let path_res = path.strip_prefix(base);
    if let Err(e) = path_res {
        return triple(&e);
    }

    let blame_res = repo.blame_file(path_res.unwrap(), None);
    if let Err(e) = blame_res {
        return triple(&e);
    }
    let blame = blame_res.unwrap();
    let id_res = blame
        .get_line(1)
        .ok_or(Error::Generic("no git history found"));

    if let Err(e) = id_res {
        return triple(&e);
    }
    let id = id_res.unwrap().orig_commit_id();

    let commit_res = repo.find_commit(id);
    if let Err(e) = commit_res {
        return triple(&e);
    }
    let commit = commit_res.unwrap();

    let time = commit.time();
    let time_return = Ok(Local.timestamp(time.seconds(), 0));

    let name_return = name_from_commit(&commit);

    let signature_return = verify_git_signature(repo, &id, store);

    (time_return, name_return, signature_return)
}

fn verify_git_signature(
    repo: &Repository,
    id: &Oid,
    store: &PasswordStore,
) -> Result<SignatureStatus> {
    let (signature, signed_data) = repo.extract_signature(id, Some("gpgsig"))?;

    let signature_str = str::from_utf8(&signature)?.to_owned();
    let signed_data_str = str::from_utf8(&signed_data)?.to_owned();

    if store.valid_gpg_signing_keys.is_empty() {
        return Err(Error::Generic(
            "signature not checked as PASSWORD_STORE_SIGNING_KEY is not configured",
        ));
    }
    match store.crypto.verify_sign(&signed_data_str.into_bytes(), &signature_str.into_bytes(), &store.valid_gpg_signing_keys) {
        Ok(r) => Ok(r),
        Err(VerificationError::InfrastructureError(message)) => Err(Error::GenericDyn(message)),
        Err(VerificationError::SignatureFromWrongRecipient) => Err(Error::Generic("the commit wasn't signed by one of the keys specified in the environmental variable PASSWORD_STORE_SIGNING_KEY")),
        Err(VerificationError::BadSignature) => Err(Error::Generic("Bad signature for commit")),
        Err(VerificationError::MissingSignatures) => Err(Error::Generic("Missing signature for commit")),
        Err(VerificationError::TooManySignatures) => Err(Error::Generic("If a git commit contains more than one signature, something is fishy")),
    }
}

/// Initialize a git repository for the store.
pub fn init_git_repo(base: &Path) -> Result<git2::Repository> {
    Ok(git2::Repository::init(base)?)
}

/// Return a list of all passwords whose name contains `query`.
pub fn search(store: &PasswordStore, query: &str) -> Result<Vec<PasswordEntry>> {
    let passwords = &store.passwords;
    fn normalized(s: &str) -> String {
        s.to_lowercase()
    }
    fn matches(s: &str, q: &str) -> bool {
        normalized(s).as_str().contains(normalized(q).as_str())
    }
    let matching = passwords.iter().filter(|p| matches(&p.name, query));
    Ok(matching.cloned().collect())
}

/// Determine password directory
pub fn password_dir(
    password_store_dir: &Option<PathBuf>,
    home: &Option<PathBuf>,
) -> Result<PathBuf> {
    let pass_home = password_dir_raw(password_store_dir, home);
    if !pass_home.exists() {
        return Err(Error::Generic("failed to locate password directory"));
    }
    Ok(pass_home)
}

/// Determine password directory
pub fn password_dir_raw(password_store_dir: &Option<PathBuf>, home: &Option<PathBuf>) -> PathBuf {
    // If a directory is provided via env var, use it
    match password_store_dir.as_ref() {
        Some(p) => p.clone(),
        None => match home {
            Some(h) => h.join(".password-store"),
            None => PathBuf::new().join(".password-store"),
        },
    }
}

fn home_exists(home: &Option<PathBuf>, settings: &config::Config) -> bool {
    if home.is_none() {
        return false;
    }
    let home = home.as_ref().unwrap();

    let home_dir = home.join(".password-store");
    if home_dir.exists() {
        if !home_dir.is_dir() {
            return false;
        }

        let stores_res = settings.get("stores");
        if let Ok(stores) = stores_res {
            let stores: HashMap<String, config::Value> = stores;

            for store_name in stores.keys() {
                let store: HashMap<String, config::Value> = stores
                    .get(store_name)
                    .unwrap()
                    .clone()
                    .into_table()
                    .unwrap();

                let password_store_dir_opt = store.get("path");
                if let Some(p) = password_store_dir_opt {
                    let p_path = PathBuf::from(p.clone().into_str().unwrap());
                    let c1 = std::fs::canonicalize(home_dir.clone());
                    let c2 = std::fs::canonicalize(p_path);
                    if c1.is_ok() && c2.is_ok() && c1.unwrap() == c2.unwrap() {
                        return false;
                    }
                }
            }
        }

        return true;
    }

    false
}

fn env_var_exists(store_dir: &Option<String>, signing_keys: &Option<String>) -> bool {
    store_dir.is_some() || signing_keys.is_some()
}

fn settings_file_exists(home: &Option<PathBuf>, xdg_config_home: &Option<PathBuf>) -> bool {
    if home.is_none() {
        return false;
    }
    let home = home.as_ref().unwrap();

    let xdg_config_file = match xdg_config_home.as_ref() {
        Some(p) => p.join("ripasso/settings.toml"),
        None => home.join(".config/ripasso/settings.toml"),
    };

    let xdg_config_file_dir = Path::new(&xdg_config_file);
    if xdg_config_file_dir.exists() {
        return match fs::metadata(xdg_config_file_dir) {
            Err(_) => false,
            Ok(config_file) => config_file.len() != 0,
        };
    }

    false
}

fn home_settings(home: &Option<PathBuf>) -> Result<config::Config> {
    let mut default_store = std::collections::HashMap::new();

    let home = home.as_ref().ok_or("no home directory set")?;

    default_store.insert(
        "path".to_owned(),
        home.join(".password-store/").to_string_lossy().to_string(),
    );

    let mut stores_map = std::collections::HashMap::new();
    stores_map.insert("default".to_owned(), default_store);

    let mut new_settings = config::Config::default();
    new_settings.set("stores", stores_map)?;

    Ok(new_settings)
}

fn var_settings(
    store_dir: &Option<String>,
    signing_keys: &Option<String>,
) -> Result<config::Config> {
    let mut default_store = std::collections::HashMap::new();

    if let Some(dir) = store_dir {
        if dir.ends_with('/') {
            default_store.insert("path".to_owned(), dir.clone());
        } else {
            default_store.insert("path".to_owned(), dir.clone() + "/");
        }
    }
    if let Some(keys) = signing_keys {
        default_store.insert("valid_signing_keys".to_owned(), keys.clone());
    } else {
        default_store.insert("valid_signing_keys".to_owned(), "-1".to_owned());
    }

    let mut stores_map = std::collections::HashMap::new();
    stores_map.insert("default".to_owned(), default_store);

    let mut new_settings = config::Config::default();
    new_settings.set("stores", stores_map)?;

    Ok(new_settings)
}

fn xdg_config_file_location(
    home: &Option<PathBuf>,
    xdg_config_home: &Option<PathBuf>,
) -> Result<PathBuf> {
    match xdg_config_home.as_ref() {
        Some(p) => Ok(p.join("ripasso/settings.toml")),
        None => {
            if let Some(h) = home {
                Ok(h.join(".config/ripasso/settings.toml"))
            } else {
                Err(Error::Generic("no home directory"))
            }
        }
    }
}

fn file_settings(xdg_config_file: &Path) -> config::File<config::FileSourceFile> {
    config::File::from(xdg_config_file.to_path_buf())
}

fn append_extension(path: PathBuf, extension: &str) -> PathBuf {
    let mut str = path.into_os_string();
    str.push(extension);
    PathBuf::from(str)
}

/// reads ripassos config file, in `$XDG_CONFIG_HOME/ripasso/settings.toml`
pub fn read_config(
    store_dir: &Option<String>,
    signing_keys: &Option<String>,
    home: &Option<PathBuf>,
    xdg_config_home: &Option<PathBuf>,
) -> Result<(config::Config, PathBuf)> {
    let mut settings = config::Config::default();
    let config_file_location = xdg_config_file_location(home, xdg_config_home)?;

    if settings_file_exists(home, xdg_config_home) {
        settings.merge(file_settings(&config_file_location))?;
    }

    if home_exists(home, &settings) {
        settings.merge(home_settings(home)?)?;
    }

    if env_var_exists(store_dir, signing_keys) {
        settings.merge(var_settings(store_dir, signing_keys)?)?;
    }

    Ok((settings, config_file_location))
}

pub fn save_config(
    stores: Arc<Mutex<Vec<Arc<Mutex<PasswordStore>>>>>,
    config_file_location: &Path,
) -> Result<()> {
    let mut stores_map = std::collections::HashMap::new();
    let stores_borrowed = stores.lock().unwrap();
    for store in stores_borrowed.iter() {
        let store = store.lock().unwrap();
        let mut store_map = std::collections::HashMap::new();
        store_map.insert(
            "path",
            store
                .get_store_path()
                .to_string_lossy()
                .into_owned()
                .to_string(),
        );
        if !store.get_valid_gpg_signing_keys().is_empty() {
            store_map.insert(
                "valid_signing_keys",
                store
                    .get_valid_gpg_signing_keys()
                    .iter()
                    .map(hex::encode_upper)
                    .collect::<Vec<String>>()
                    .join(","),
            );
        }
        if let Some(style_file) = store.get_style_file() {
            store_map.insert("style_path", style_file.display().to_string());
        }
        stores_map.insert(store.get_name().clone(), store_map);
    }

    let mut settings = std::collections::HashMap::new();
    settings.insert("stores", stores_map);

    let f = std::fs::File::create(config_file_location)?;
    let mut f = std::io::BufWriter::new(f);
    f.write_all(toml::ser::to_string_pretty(&settings)?.as_bytes())?;

    Ok(())
}

#[cfg(test)]
#[path = "tests/pass.rs"]
mod pass_tests;
