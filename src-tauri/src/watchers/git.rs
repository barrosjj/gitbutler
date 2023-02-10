use crate::{fs, projects::Project, sessions};
use filetime::FileTime;
use git2::{IndexTime, Repository};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{BufReader, Read},
    os::unix::prelude::MetadataExt,
    path::Path,
    thread,
    time::{Duration, SystemTime},
};

#[derive(Debug)]
pub enum WatchError {
    GitError(git2::Error),
    IOError(std::io::Error),
}

impl std::fmt::Display for WatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WatchError::GitError(e) => write!(f, "Git error: {}", e),
            WatchError::IOError(e) => write!(f, "IO error: {}", e),
        }
    }
}

impl From<git2::Error> for WatchError {
    fn from(error: git2::Error) -> Self {
        Self::GitError(error)
    }
}

impl From<std::io::Error> for WatchError {
    fn from(error: std::io::Error) -> Self {
        Self::IOError(error)
    }
}

const FIVE_MINUTES: u64 = Duration::new(5 * 60, 0).as_secs();
const ONE_HOUR: u64 = Duration::new(60 * 60, 0).as_secs();

pub fn watch<R: tauri::Runtime>(
    window: tauri::Window<R>,
    project: Project,
) -> Result<(), WatchError> {
    let repo = git2::Repository::open(&project.path)?;
    thread::spawn(move || loop {
        match check_for_changes(&repo) {
            Ok(Some(session)) => {
                let event_name = format!("project://{}/sessions", project.id);
                match window.emit(&event_name, &session) {
                    Ok(_) => {}
                    Err(e) => log::error!("Error: {:?}", e),
                };
            }
            Ok(None) => {}
            Err(error) => {
                log::error!(
                    "Error while checking {} for changes: {}",
                    repo.workdir().unwrap().display(),
                    error
                );
            }
        }
        thread::sleep(Duration::from_secs(10));
    });

    Ok(())
}

// main thing called in a loop to check for changes and write our custom commit data
// it will commit only if there are changes and the session is either idle for 5 minutes or is over an hour old
// currently it looks at every file in the wd, but we should probably just look at the ones that have changed when we're certain we can get everything
// - however, it does compare to the git index so we don't actually have to read the contents of every file, so maybe it's not too slow unless in huge repos
// - also only does the file comparison on commit, so it's not too bad
//
// returns a commited session if crated
fn check_for_changes(
    repo: &Repository,
) -> Result<Option<sessions::Session>, Box<dyn std::error::Error>> {
    if ready_to_commit(repo)? {
        let wd_index = &mut git2::Index::new()?;
        build_wd_index(&repo, wd_index)?;
        let wd_tree = wd_index.write_tree_to(&repo)?;

        let session_index = &mut git2::Index::new()?;
        build_session_index(&repo, session_index)?;
        let session_tree = session_index.write_tree_to(&repo)?;

        let log_index = &mut git2::Index::new()?;
        build_log_index(&repo, log_index)?;
        let log_tree = log_index.write_tree_to(&repo)?;

        let mut tree_builder = repo.treebuilder(None)?;
        tree_builder.insert("session", session_tree, 0o040000)?;
        tree_builder.insert("wd", wd_tree, 0o040000)?;
        tree_builder.insert("logs", log_tree, 0o040000)?;

        let tree = tree_builder.write()?;

        let commit_oid = write_gb_commit(tree, &repo)?;
        log::debug!(
            "{}: wrote gb commit {}",
            repo.workdir().unwrap().display(),
            commit_oid
        );
        sessions::delete_current_session(repo)?;

        let commit = repo.find_commit(commit_oid)?;
        let session = sessions::Session::from_commit(repo, &commit)?;

        Ok(Some(session))
    } else {
        Ok(None)
    }

    // TODO: try to push the new gb history head to the remote
    // TODO: if we see it is not a FF, pull down the remote, determine order, rewrite the commit line, and push again
}

// make sure that the .git/gb/session directory exists (a session is in progress)
// and that there has been no activity in the last 5 minutes (the session appears to be over)
// and the start was at most an hour ago
fn ready_to_commit(repo: &Repository) -> Result<bool, Box<dyn std::error::Error>> {
    if let Some(current_session) = sessions::Session::current(repo)? {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as u64;

        let elapsed_last = now - current_session.meta.last_ts;
        let elapsed_start = now - current_session.meta.start_ts;

        // TODO: uncomment
        if (elapsed_last > FIVE_MINUTES) || (elapsed_start > ONE_HOUR) {
            Ok(true)
        } else {
            log::debug!(
                "Not ready to commit {} yet. ({} seconds elapsed, {} seconds since start)",
                repo.workdir().unwrap().display(),
                elapsed_last,
                elapsed_start
            );
            Ok(false)
        }
    } else {
        log::debug!(
            "No current session for {}",
            repo.workdir().unwrap().display()
        );
        Ok(false)
    }
}

// build the initial tree from the working directory, not taking into account the gitbutler metadata
// eventually we might just want to run this once and then update it with the files that are changed over time, but right now we're running it every commit
// it ignores files that are in the .gitignore
fn build_wd_index(
    repo: &Repository,
    index: &mut git2::Index,
) -> Result<(), Box<dyn std::error::Error>> {
    // create a new in-memory git2 index and open the working one so we can cheat if none of the metadata of an entry has changed
    let repo_index = &mut repo.index()?;

    // add all files in the working directory to the in-memory index, skipping for matching entries in the repo index
    let all_files = fs::list_files(repo.workdir().unwrap())?;
    for file in all_files {
        let file_path = Path::new(&file);
        if !repo.is_path_ignored(&file).unwrap_or(true) {
            add_path(index, repo_index, &file_path, &repo)?;
        }
    }

    Ok(())
}

// take a file path we see and add it to our in-memory index
// we call this from build_initial_wd_tree, which is smart about using the existing index to avoid rehashing files that haven't changed
// and also looks for large files and puts in a placeholder hash in the LFS format
// TODO: actually upload the file to LFS
fn add_path(
    index: &mut git2::Index,
    repo_index: &mut git2::Index,
    rel_file_path: &Path,
    repo: &Repository,
) -> Result<(), Box<dyn std::error::Error>> {
    let abs_file_path = repo.workdir().unwrap().join(rel_file_path);
    let file_path = Path::new(&abs_file_path);

    let metadata = file_path.metadata()?;
    let mtime = FileTime::from_last_modification_time(&metadata);
    let ctime = FileTime::from_creation_time(&metadata).unwrap();

    // if we find the entry in the index, we can just use it
    match repo_index.get_path(rel_file_path, 0) {
        // if we find the entry and the metadata of the file has not changed, we can just use the existing entry
        Some(entry) => {
            if entry.mtime.seconds() == i32::try_from(mtime.seconds())?
                && entry.mtime.nanoseconds() == u32::try_from(mtime.nanoseconds())?
                && entry.file_size == u32::try_from(metadata.len())?
                && entry.mode == metadata.mode()
            {
                log::debug!("Using existing entry for {}", file_path.display());
                index.add(&entry).unwrap();
                return Ok(());
            }
        }
        None => {
            log::debug!("No entry found for {}", file_path.display());
        }
    };

    // something is different, or not found, so we need to create a new entry

    log::debug!("Adding wd path: {}", file_path.display());

    // look for files that are bigger than 4GB, which are not supported by git
    // insert a pointer as the blob content instead
    // TODO: size limit should be configurable
    let blob = if metadata.len() > 100_000_000 {
        log::debug!(
            "{}: file too big: {}",
            repo.workdir().unwrap().display(),
            file_path.display()
        );

        // get a sha256 hash of the file first
        let sha = sha256_digest(&file_path)?;

        // put togther a git lfs pointer file: https://github.com/git-lfs/git-lfs/blob/main/docs/spec.md
        let mut lfs_pointer = String::from("version https://git-lfs.github.com/spec/v1\n");
        lfs_pointer.push_str("oid sha256:");
        lfs_pointer.push_str(&sha);
        lfs_pointer.push_str("\n");
        lfs_pointer.push_str("size ");
        lfs_pointer.push_str(&metadata.len().to_string());
        lfs_pointer.push_str("\n");

        // write the file to the .git/lfs/objects directory
        // create the directory recursively if it doesn't exist
        let lfs_objects_dir = repo.path().join("lfs/objects");
        std::fs::create_dir_all(lfs_objects_dir.clone())?;
        let lfs_path = lfs_objects_dir.join(sha);
        std::fs::copy(file_path, lfs_path)?;

        repo.blob(lfs_pointer.as_bytes()).unwrap()
    } else {
        // read the file into a blob, get the object id
        repo.blob_path(&file_path)?
    };

    // create a new IndexEntry from the file metadata
    index.add(&git2::IndexEntry {
        ctime: IndexTime::new(ctime.seconds().try_into()?, ctime.nanoseconds().try_into()?),
        mtime: IndexTime::new(mtime.seconds().try_into()?, mtime.nanoseconds().try_into()?),
        dev: metadata.dev().try_into()?,
        ino: metadata.ino().try_into()?,
        mode: metadata.mode(),
        uid: metadata.uid().try_into()?,
        gid: metadata.gid().try_into()?,
        file_size: metadata.len().try_into()?,
        flags: 10, // normal flags for normal file (for the curious: https://git-scm.com/docs/index-format)
        flags_extended: 0, // no extended flags
        path: rel_file_path.to_str().unwrap().to_string().into(),
        id: blob,
    })?;

    Ok(())
}

/// calculates sha256 digest of a large file as lowercase hex string via streaming buffer
/// used to calculate the hash of large files that are not supported by git
fn sha256_digest(path: &Path) -> Result<String, std::io::Error> {
    let input = File::open(path)?;
    let mut reader = BufReader::new(input);

    let digest = {
        let mut hasher = Sha256::new();
        let mut buffer = [0; 1024];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        hasher.finalize()
    };
    Ok(format!("{:X}", digest))
}

fn build_log_index(
    repo: &Repository,
    index: &mut git2::Index,
) -> Result<(), Box<dyn std::error::Error>> {
    let log_path = repo.path().join("logs/HEAD");
    log::debug!("Adding log path: {}", log_path.display());

    let metadata = log_path.metadata()?;
    let mtime = FileTime::from_last_modification_time(&metadata);
    let ctime = FileTime::from_creation_time(&metadata).unwrap();

    index.add(&git2::IndexEntry {
        ctime: IndexTime::new(ctime.seconds().try_into()?, ctime.nanoseconds().try_into()?),
        mtime: IndexTime::new(mtime.seconds().try_into()?, mtime.nanoseconds().try_into()?),
        dev: metadata.dev().try_into()?,
        ino: metadata.ino().try_into()?,
        mode: metadata.mode(),
        uid: metadata.uid().try_into()?,
        gid: metadata.gid().try_into()?,
        file_size: metadata.len().try_into()?,
        flags: 10, // normal flags for normal file (for the curious: https://git-scm.com/docs/index-format)
        flags_extended: 0, // no extended flags
        path: "HEAD".to_string().into(),
        id: repo.blob_path(&log_path)?,
    })?;

    Ok(())
}

fn build_session_index(
    repo: &Repository,
    index: &mut git2::Index,
) -> Result<(), Box<dyn std::error::Error>> {
    // add all files in the working directory to the in-memory index, skipping for matching entries in the repo index
    let session_dir = repo.path().join("gb/session");
    for session_file in fs::list_files(&session_dir)? {
        let file_path = Path::new(&session_file);
        add_session_path(&repo, index, &file_path)?;
    }

    Ok(())
}

// this is a helper function for build_gb_tree that takes paths under .git/gb/session and adds them to the in-memory index
fn add_session_path(
    repo: &Repository,
    index: &mut git2::Index,
    rel_file_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let file_path = repo.path().join("gb/session").join(rel_file_path);

    log::debug!("Adding session path: {}", file_path.display());

    let blob = repo.blob_path(&file_path)?;
    let metadata = file_path.metadata()?;
    let mtime = FileTime::from_last_modification_time(&metadata);
    let ctime = FileTime::from_creation_time(&metadata).unwrap();

    // create a new IndexEntry from the file metadata
    index.add(&git2::IndexEntry {
        ctime: IndexTime::new(ctime.seconds().try_into()?, ctime.nanoseconds().try_into()?),
        mtime: IndexTime::new(mtime.seconds().try_into()?, mtime.nanoseconds().try_into()?),
        dev: metadata.dev().try_into()?,
        ino: metadata.ino().try_into()?,
        mode: metadata.mode(),
        uid: metadata.uid().try_into()?,
        gid: metadata.gid().try_into()?,
        file_size: metadata.len().try_into()?,
        flags: 10, // normal flags for normal file (for the curious: https://git-scm.com/docs/index-format)
        flags_extended: 0, // no extended flags
        path: rel_file_path.to_str().unwrap().into(),
        id: blob,
    })?;

    Ok(())
}

// write a new commit object to the repo
// this is called once we have a tree of deltas, metadata and current wd snapshot
// and either creates or updates the refs/gitbutler/current ref
fn write_gb_commit(gb_tree: git2::Oid, repo: &Repository) -> Result<git2::Oid, git2::Error> {
    // find the Oid of the commit that refs/gitbutler/current points to, none if it doesn't exist
    match repo.revparse_single("refs/gitbutler/current") {
        Ok(obj) => {
            let last_commit = repo.find_commit(obj.id()).unwrap();
            let new_commit = repo.commit(
                Some("refs/gitbutler/current"),
                &repo.signature().unwrap(),        // author
                &repo.signature().unwrap(),        // committer
                "gitbutler check",                 // commit message
                &repo.find_tree(gb_tree).unwrap(), // tree
                &[&last_commit],                   // parents
            )?;
            Ok(new_commit)
        }
        Err(_) => {
            let new_commit = repo.commit(
                Some("refs/gitbutler/current"),
                &repo.signature().unwrap(),        // author
                &repo.signature().unwrap(),        // committer
                "gitbutler check",                 // commit message
                &repo.find_tree(gb_tree).unwrap(), // tree
                &[],                               // parents
            )?;
            Ok(new_commit)
        }
    }
}
